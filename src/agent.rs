use anyhow::{Context, Result};
use log::debug;
use std::io::{IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use strum::{Display, EnumString};

use crate::anthropic::{
    CacheControl, Client, ContentBlock, CustomTool, FetchToolType, Message, MessagesRequest, Role,
    ServerTool, StopReason, SystemBlock, SystemPrompt, Tool, WebSearchToolType,
};
use crate::config::Model;
use crate::llm_cache::LlmCache;

const MAX_TOKENS: u32 = 4096;
const AGENTS_MD_PATH: &str = "AGENTS.md";

const BASE_SYSTEM_PROMPT: &str = "You are a helpful assistant running inside a sandboxed environment. You can execute bash commands to help the user.";

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

fn websearch_tool() -> Tool {
    Tool::Server(ServerTool::WebSearch {
        tool_type: WebSearchToolType::WebSearch20250305,
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
        user_location: None,
    })
}

fn fetch_tool() -> Tool {
    Tool::Server(ServerTool::WebFetch {
        tool_type: FetchToolType::WebFetch20250910,
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
    })
}

/// Read AGENTS.md from the sandbox if it exists.
fn read_agents_md(container_name: &str) -> Option<String> {
    debug!("Reading {} from sandbox", AGENTS_MD_PATH);
    let output = Command::new("docker")
        .args(["exec", container_name, "cat", AGENTS_MD_PATH])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        debug!("{} not found or not readable", AGENTS_MD_PATH);
        return None;
    }

    debug!("{} loaded successfully", AGENTS_MD_PATH);
    String::from_utf8(output.stdout).ok()
}

fn build_system_prompt(agents_md: Option<&str>) -> String {
    match agents_md {
        Some(content) => format!("{}\n\n{}", BASE_SYSTEM_PROMPT, content),
        None => BASE_SYSTEM_PROMPT.to_string(),
    }
}

fn execute_edit_in_sandbox(
    container_name: &str,
    file_path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<(String, bool)> {
    debug!("Reading file for edit: {}", file_path);
    let output = Command::new("docker")
        .args(["exec", container_name, "cat", file_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to read file in sandbox")?;
    debug!("File read completed");

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

    debug!("Writing edited file: {}", file_path);
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

    debug!("Waiting for write process to complete");
    let output = write_process
        .wait_with_output()
        .context("Failed to wait for write process")?;
    debug!("Write process completed with status: {:?}", output.status);

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
    debug!("Checking if file exists: {}", file_path);
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
            debug!("Creating parent directories for: {}", file_path);
            let _ = Command::new("docker")
                .args(["exec", container_name, "bash", "-c", &mkdir_cmd])
                .output();
        }
    }

    debug!("Writing new file: {}", file_path);
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

    debug!("Waiting for write process to complete");
    let output = write_process
        .wait_with_output()
        .context("Failed to wait for write process")?;
    debug!("Write process completed with status: {:?}", output.status);

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
    debug!("Saving large output to file: {}", output_file);

    // Create /agent directory if it doesn't exist
    debug!("Creating /agent directory");
    Command::new("docker")
        .args(["exec", container_name, "bash", "-c", "mkdir -p /agent"])
        .output()
        .context("Failed to create /agent directory")?;

    // Write the output to file
    debug!("Writing output data ({} bytes)", data.len());
    let write_cmd = format!("cat > {}", output_file);
    let mut write_process = Command::new("docker")
        .args(["exec", "-i", container_name, "bash", "-c", &write_cmd])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to write output to file")?;

    let mut stdin = write_process
        .stdin
        .take()
        .expect("Process was launched with piped stdin");
    stdin.write_all(data).context("Failed to write to stdin")?;
    drop(stdin);

    debug!("Waiting for output save process to complete");
    write_process
        .wait()
        .context("Failed to wait for write process")?;
    debug!("Output saved to file");

    Ok(output_file)
}

/// Prompts user to confirm exit when they submit empty input.
/// Returns true if user wants to exit (Enter or 'y'), false otherwise.
fn confirm_exit() -> Result<bool> {
    eprint!("Exit? [Y/n] ");
    std::io::stderr().flush()?;

    let mut buf = [0u8; 1];
    let bytes_read = std::io::stdin().read(&mut buf)?;

    if bytes_read == 0 || buf[0] == b'\n' || buf[0] == b'y' || buf[0] == b'Y' {
        return Ok(true);
    }

    // Discard remaining input so it doesn't leak to the next prompt
    let mut discard = String::new();
    std::io::stdin().read_line(&mut discard)?;

    Ok(false)
}

/// Get user input by launching vim on a temp file containing the chat history.
/// Returns the new message (content after the chat history prefix).
/// If the user doesn't preserve the chat history prefix, prompts to retry.
fn get_input_via_vim(chat_history: &str) -> Result<String> {
    use std::fs;

    loop {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!("sandbox-chat-{}.txt", std::process::id()));

        fs::write(&temp_file, chat_history).context("Failed to write temp file for vim")?;

        let status = Command::new("vim")
            .arg(&temp_file)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to launch vim")?;

        if !status.success() {
            anyhow::bail!("vim exited with non-zero status");
        }

        let edited_content = fs::read_to_string(&temp_file).context("Failed to read temp file")?;
        let _ = fs::remove_file(&temp_file);

        // Prevent accidental editing of history
        if !edited_content.starts_with(chat_history) {
            eprintln!("Error: The chat history prefix was modified. Please keep it intact.");
            eprint!("Press Enter to try again...");
            std::io::stderr().flush()?;

            let mut buf = [0u8; 1];
            let _ = std::io::stdin().read(&mut buf);
            continue;
        }

        let new_message = edited_content[chat_history.len()..].trim().to_string();
        return Ok(new_message);
    }
}

fn execute_bash_in_sandbox(container_name: &str, command: &str) -> Result<(String, bool)> {
    const MAX_OUTPUT_SIZE: usize = 30000;

    debug!("Executing bash in sandbox: {}", command);
    let output = Command::new("docker")
        .args(["exec", container_name, "bash", "-c", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to execute command in sandbox")?;
    debug!("Bash command completed with status: {:?}", output.status);

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

/// Helper macro to append to chat history and print to stdout
macro_rules! chat_println {
    ($history:expr) => {{
        println!();
        $history.push('\n');
    }};
    ($history:expr, $($arg:tt)*) => {{
        let s = format!($($arg)*);
        println!("{}", s);
        $history.push_str(&s);
        $history.push('\n');
    }};
}

pub fn run_agent(container_name: &str, model: Model, cache: Option<LlmCache>) -> Result<()> {
    let client = Client::new_with_cache(cache)?;

    let mut stdout = std::io::stdout();

    let mut messages: Vec<Message> = Vec::new();
    let mut chat_history = String::new();

    // Read AGENTS.md once at startup to include project-specific instructions
    let agents_md = read_agents_md(container_name);
    let system_prompt = build_system_prompt(agents_md.as_deref());

    let is_tty = std::io::stdin().is_terminal();

    // Non-TTY mode reads entire stdin upfront and exits after one response
    let initial_prompt = if !is_tty {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("Failed to read stdin")?;
        Some(input.trim().to_string())
    } else {
        None
    };

    loop {
        let user_input = if let Some(ref prompt) = initial_prompt {
            if !messages.is_empty() {
                break;
            }
            prompt.clone()
        } else {
            let input = get_input_via_vim(&chat_history)?;
            if input.is_empty() {
                if confirm_exit()? {
                    break;
                }
                continue;
            }
            input
        };

        chat_println!(chat_history, "> {}", user_input);
        stdout.flush()?;

        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: user_input,
                cache_control: None,
            }],
        });

        loop {
            // Cache conversation history by marking the last content block.
            // Single breakpoint at the end is optimal for non-rewinding multi-turn agents.
            let mut request_messages = messages.clone();
            if let Some(last_msg) = request_messages.last_mut() {
                if last_msg.role == Role::User {
                    if let Some(last_content) = last_msg.content.last_mut() {
                        match last_content {
                            ContentBlock::Text { cache_control, .. } => {
                                *cache_control = Some(CacheControl::default());
                            }
                            ContentBlock::ToolResult { cache_control, .. } => {
                                *cache_control = Some(CacheControl::default());
                            }
                            _ => {}
                        }
                    }
                }
            }

            let request = MessagesRequest {
                model: model.api_model_id().to_string(),
                max_tokens: MAX_TOKENS,
                system: Some(SystemPrompt::Blocks(vec![SystemBlock::Text {
                    text: system_prompt.clone(),
                    cache_control: Some(CacheControl::default()),
                }])),
                messages: request_messages,
                tools: Some(vec![
                    bash_tool(),
                    edit_tool(),
                    write_tool(),
                    websearch_tool(),
                    fetch_tool(),
                ]),
                temperature: None,
                top_p: None,
                top_k: None,
            };

            let response = client.messages(request)?;

            let mut has_tool_use = false;
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for block in &response.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        chat_println!(chat_history, "{}", text);
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        has_tool_use = true;
                        let tool_name = AgentToolName::from_str(name)
                            .map_err(|_| anyhow::anyhow!("Unknown tool: {}", name))?;

                        let (output, success) = match tool_name {
                            AgentToolName::Bash => {
                                let command =
                                    input.get("command").and_then(|v| v.as_str()).unwrap_or("");

                                chat_println!(chat_history, "$ {}", command);

                                let (output, success) =
                                    execute_bash_in_sandbox(container_name, command)?;

                                if !output.is_empty() {
                                    chat_println!(chat_history, "{}", output);
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

                                let (output, success) = execute_edit_in_sandbox(
                                    container_name,
                                    file_path,
                                    old_string,
                                    new_string,
                                )?;

                                if success {
                                    chat_println!(chat_history, "[edit] {}", file_path);
                                } else {
                                    chat_println!(chat_history, "[edit] {} (failed)", file_path);
                                    chat_println!(chat_history, "{}", output);
                                }
                                (output, success)
                            }
                            AgentToolName::Write => {
                                let file_path = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let content =
                                    input.get("content").and_then(|v| v.as_str()).unwrap_or("");

                                let (output, success) =
                                    execute_write_in_sandbox(container_name, file_path, content)?;

                                if success {
                                    chat_println!(chat_history, "[write] {}", file_path);
                                } else {
                                    chat_println!(chat_history, "[write] {} (failed)", file_path);
                                    chat_println!(chat_history, "{}", output);
                                }
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
                    // Server-side tools (web_search, web_fetch) are handled by the API
                    ContentBlock::ServerToolUse { name, input, .. } => {
                        if name == "web_search" {
                            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                            chat_println!(chat_history, "[search] {}", query);
                        } else if name == "web_fetch" {
                            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            chat_println!(chat_history, "[fetch] {}", url);
                        }
                    }
                    ContentBlock::WebSearchToolResult { .. } => {}
                    ContentBlock::WebFetchToolResult { content, .. } => {
                        if let crate::anthropic::WebFetchResult::WebFetchToolError { error_code } =
                            content
                        {
                            chat_println!(chat_history, "[fetch] (failed: {})", error_code);
                        }
                    }
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
    }

    Ok(())
}
