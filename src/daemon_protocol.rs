//! JSON-RPC protocol for communication between sandbox clients and daemon.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crate::config::{OverlayMode, Runtime, UserInfo};

/// Parameters needed to start a sandbox container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxParams {
    pub project_dir: PathBuf,
    pub image_tag: String,
    pub user_info: UserInfoWire,
    pub runtime: RuntimeWire,
    pub overlay_mode: OverlayModeWire,
    pub env_vars: Vec<(String, String)>,
}

/// Wire format for UserInfo (serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfoWire {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub shell: String,
}

impl From<&UserInfo> for UserInfoWire {
    fn from(u: &UserInfo) -> Self {
        Self {
            username: u.username.clone(),
            uid: u.uid,
            gid: u.gid,
            shell: u.shell.clone(),
        }
    }
}

impl From<UserInfoWire> for UserInfo {
    fn from(w: UserInfoWire) -> Self {
        Self {
            username: w.username,
            uid: w.uid,
            gid: w.gid,
            shell: w.shell,
        }
    }
}

/// Wire format for Runtime (serializable).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeWire {
    Runsc,
    Runc,
    SysboxRunc,
}

impl From<Runtime> for RuntimeWire {
    fn from(r: Runtime) -> Self {
        match r {
            Runtime::Runsc => Self::Runsc,
            Runtime::Runc => Self::Runc,
            Runtime::SysboxRunc => Self::SysboxRunc,
        }
    }
}

impl From<RuntimeWire> for Runtime {
    fn from(w: RuntimeWire) -> Self {
        match w {
            RuntimeWire::Runsc => Self::Runsc,
            RuntimeWire::Runc => Self::Runc,
            RuntimeWire::SysboxRunc => Self::SysboxRunc,
        }
    }
}

/// Wire format for OverlayMode (serializable).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayModeWire {
    Overlayfs,
    Copy,
}

impl From<OverlayMode> for OverlayModeWire {
    fn from(m: OverlayMode) -> Self {
        match m {
            OverlayMode::Overlayfs => Self::Overlayfs,
            OverlayMode::Copy => Self::Copy,
        }
    }
}

impl From<OverlayModeWire> for OverlayMode {
    fn from(w: OverlayModeWire) -> Self {
        match w {
            OverlayModeWire::Overlayfs => Self::Overlayfs,
            OverlayModeWire::Copy => Self::Copy,
        }
    }
}

/// Daemon RPC API.
pub trait DaemonApi {
    /// Ensure the sandbox is running. Blocks until the container is started.
    fn ensure_sandbox(&mut self, sandbox_name: &str, params: &SandboxParams) -> Result<()>;
}

/// Client implementation of the daemon API over a stream.
pub struct Client<S> {
    stream: S,
}

impl<S> Client<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    /// Consume the client and return the underlying stream.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: std::io::Read + Write> DaemonApi for Client<S> {
    fn ensure_sandbox(&mut self, sandbox_name: &str, params: &SandboxParams) -> Result<()> {
        let request = Request {
            method: Method::EnsureSandbox,
            params: Some(RequestParams::EnsureSandbox(EnsureSandboxParams {
                sandbox_name: sandbox_name.to_string(),
                params: params.clone(),
            })),
        };

        send_request(&mut self.stream, &request)?;
        let response = read_response(&mut self.stream)?;

        if let Some(err) = response.error {
            bail!("Daemon error: {} (code {})", err.message, err.code);
        }

        Ok(())
    }
}

fn send_request(stream: &mut impl Write, request: &Request) -> Result<()> {
    let mut json = serde_json::to_string(request)?;
    json.push('\n');
    stream
        .write_all(json.as_bytes())
        .context("Failed to send request to daemon")?;
    Ok(())
}

fn read_response(stream: &mut impl std::io::Read) -> Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("Failed to read response from daemon")?;

    if line.is_empty() {
        bail!("Daemon closed connection before responding");
    }

    serde_json::from_str(&line).context("Failed to parse daemon response")
}

// --- Wire format types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    EnsureSandbox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    method: Method,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<RequestParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RequestParams {
    EnsureSandbox(EnsureSandboxParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnsureSandboxParams {
    sandbox_name: String,
    params: SandboxParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<ResponseResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ResponseResult {
    EnsureSandbox(EnsureSandboxResult),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EnsureSandboxResult {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl Response {
    fn success(result: ResponseResult) -> Self {
        Self {
            result: Some(result),
            error: None,
        }
    }

    fn error(code: i32, message: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Server-side request handling.
pub mod server {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    /// A parsed client request.
    #[derive(Debug)]
    pub enum ClientRequest {
        EnsureSandbox {
            sandbox_name: String,
            params: SandboxParams,
        },
    }

    /// Read and parse a request from a client stream.
    pub fn read_request(stream: &mut impl std::io::Read) -> Result<ClientRequest> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("Failed to read request from client")?;

        if line.is_empty() {
            bail!("Client closed connection before sending request");
        }

        let request: Request =
            serde_json::from_str(&line).context("Failed to parse client request")?;

        match request.method {
            Method::EnsureSandbox => {
                let params = match request.params {
                    Some(RequestParams::EnsureSandbox(p)) => p,
                    None => bail!("Missing params for ensure_sandbox"),
                };
                Ok(ClientRequest::EnsureSandbox {
                    sandbox_name: params.sandbox_name,
                    params: params.params,
                })
            }
        }
    }

    /// Send a success response for ensure_sandbox.
    pub fn send_ensure_sandbox_ok(stream: &mut impl Write) -> Result<()> {
        let response = Response::success(ResponseResult::EnsureSandbox(EnsureSandboxResult {}));
        send_response(stream, &response)
    }

    /// Send an error response.
    pub fn send_error(stream: &mut impl Write, code: i32, message: &str) -> Result<()> {
        let response = Response::error(code, message);
        send_response(stream, &response)
    }

    fn send_response(stream: &mut impl Write, response: &Response) -> Result<()> {
        let mut json = serde_json::to_string(response)?;
        json.push('\n');
        stream.write_all(json.as_bytes())?;
        Ok(())
    }
}
