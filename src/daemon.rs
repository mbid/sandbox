use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{OverlayMode, Runtime, UserInfo};
use crate::daemon_protocol::{self, server, SandboxParams};
use crate::docker;
use crate::git;
use crate::sandbox::SandboxInfo;
use crate::sandbox_config::SandboxConfig;

/// Environment variable to override the daemon socket path (for testing).
pub const SOCKET_PATH_ENV: &str = "SANDBOX_DAEMON_SOCKET";

/// Get the daemon socket path.
/// Uses $SANDBOX_DAEMON_SOCKET if set, otherwise $XDG_RUNTIME_DIR/sandbox.sock.
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(SOCKET_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR not set; cannot determine daemon socket path")?;
    Ok(PathBuf::from(runtime_dir).join("sandbox.sock"))
}

/// Handle to a daemon connection. Dropping this signals disconnection to the daemon.
pub struct DaemonConnection {
    // Hold the stream to keep the connection alive; the daemon tracks
    // connections and cleans up sandboxes when all clients disconnect.
    #[allow(dead_code)]
    stream: UnixStream,
}

/// Connect to the daemon and ensure a sandbox is running.
/// Returns an error if the daemon is not running.
pub fn connect(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<DaemonConnection> {
    let sock_path = socket_path()?;

    let params = SandboxParams {
        project_dir: info.repo_root.clone(),
        image_tag: image_tag.to_string(),
        user_info: user_info.into(),
        runtime: runtime.into(),
        overlay_mode: overlay_mode.into(),
        env_vars: env_vars.to_vec(),
    };

    let stream = UnixStream::connect(&sock_path).with_context(|| {
        format!(
            "Failed to connect to daemon at {}. Is the daemon running?",
            sock_path.display()
        )
    })?;

    debug!("Connected to daemon");
    let stream = do_handshake(stream, &info.name, &params)?;
    Ok(DaemonConnection { stream })
}

fn do_handshake(
    stream: UnixStream,
    sandbox_name: &str,
    params: &SandboxParams,
) -> Result<UnixStream> {
    use daemon_protocol::DaemonApi;
    let mut client = daemon_protocol::Client::new(stream);
    client.ensure_sandbox(sandbox_name, params)?;
    Ok(client.into_inner())
}

// --- Git Sync Thread ---

/// Message type for controlling the git sync thread.
enum GitSyncMessage {
    /// Signal to stop the thread gracefully.
    Stop,
}

/// Handle to a running git sync thread that can be stopped gracefully.
pub struct GitSyncThread {
    stop_tx: Sender<GitSyncMessage>,
    handle: Option<JoinHandle<()>>,
}

impl GitSyncThread {
    /// Spawn a new git sync thread for the given sandbox.
    pub fn spawn(info: SandboxInfo) -> Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            if let Err(e) = run_git_sync_loop(info, stop_rx) {
                error!("Git sync thread failed: {:#}", e);
            }
        });

        Ok(GitSyncThread {
            stop_tx,
            handle: Some(handle),
        })
    }

    /// Stop the git sync thread gracefully and wait for it to finish.
    pub fn stop(mut self) {
        let _ = self.stop_tx.send(GitSyncMessage::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for GitSyncThread {
    fn drop(&mut self) {
        // Send stop signal but don't wait (might already be stopped)
        let _ = self.stop_tx.send(GitSyncMessage::Stop);
    }
}

fn run_full_git_sync(info: &SandboxInfo) -> Result<()> {
    git::sync_sandbox_to_meta(&info.meta_git_dir, &info.clone_dir, &info.name)
        .context("syncing sandbox to meta.git")?;
    git::sync_main_to_meta(&info.repo_root, &info.meta_git_dir)
        .context("syncing main branch to meta.git")?;
    git::sync_meta_to_host(&info.repo_root, &info.meta_git_dir, &info.name)
        .context("syncing meta.git to host")?;
    git::sync_meta_to_sandbox(&info.meta_git_dir, &info.clone_dir, &info.name)
        .context("syncing meta.git to sandbox")?;
    Ok(())
}

fn run_git_sync_loop(info: SandboxInfo, stop_rx: mpsc::Receiver<GitSyncMessage>) -> Result<()> {
    let debounce = Duration::from_millis(500);
    let mut last_sync = Instant::now();
    let mut pending_sync = false;

    let (watcher_tx, watcher_rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = watcher_tx.send(res);
        },
        Config::default(),
    )
    .context("creating watcher")?;

    // Watch the three refs/heads directories with non-recursive watchers
    let host_refs = info.repo_root.join(".git/refs/heads");
    let meta_refs = info.meta_git_dir.join("refs/heads");
    let sandbox_refs = info.clone_dir.join(".git/refs/heads");

    std::fs::create_dir_all(&host_refs)
        .with_context(|| format!("creating {}", host_refs.display()))?;
    std::fs::create_dir_all(&meta_refs)
        .with_context(|| format!("creating {}", meta_refs.display()))?;
    std::fs::create_dir_all(&sandbox_refs)
        .with_context(|| format!("creating {}", sandbox_refs.display()))?;

    watcher
        .watch(&host_refs, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", host_refs.display()))?;
    watcher
        .watch(&meta_refs, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", meta_refs.display()))?;
    watcher
        .watch(&sandbox_refs, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", sandbox_refs.display()))?;

    info!(
        "Git sync watching: {}, {}, {}",
        host_refs.display(),
        meta_refs.display(),
        sandbox_refs.display()
    );

    // Run initial sync
    if let Err(e) = run_full_git_sync(&info) {
        error!("Initial git sync failed: {:#}", e);
    }

    loop {
        // Check for stop signal first
        match stop_rx.try_recv() {
            Ok(GitSyncMessage::Stop) => {
                info!("Git sync thread received stop signal");
                // Run final sync before exiting
                if let Err(e) = run_full_git_sync(&info) {
                    error!("Final git sync failed: {:#}", e);
                }
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                info!("Git sync stop channel disconnected");
                return Ok(());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // Check for file system events
        match watcher_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                if !event.kind.is_access() {
                    pending_sync = true;
                }
            }
            Ok(Err(e)) => {
                error!("Watcher error: {}", e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Git sync watcher channel disconnected");
                return Ok(());
            }
        }

        let now = Instant::now();

        if pending_sync && now.duration_since(last_sync) > debounce {
            if let Err(e) = run_full_git_sync(&info) {
                error!("Git sync failed: {:#}", e);
            }
            last_sync = now;
            pending_sync = false;
        }
    }
}

// --- Daemon State ---

/// State for a single sandbox managed by the daemon.
struct SandboxState {
    info: SandboxInfo,
    git_sync: GitSyncThread,
    /// Number of active client connections for this sandbox.
    client_count: usize,
}

/// Global daemon state shared across all connection handler threads.
struct DaemonState {
    /// Active sandboxes, keyed by sandbox name.
    sandboxes: HashMap<String, SandboxState>,
}

impl DaemonState {
    fn new() -> Self {
        DaemonState {
            sandboxes: HashMap::new(),
        }
    }
}

type SharedState = Arc<Mutex<DaemonState>>;

fn start_container(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<()> {
    crate::sandbox::ensure_container_running_internal(
        info,
        image_tag,
        user_info,
        runtime,
        overlay_mode,
        env_vars,
    )
}

// --- Daemon Implementation ---

/// Unique key for identifying a sandbox (project_dir + sandbox_name).
fn sandbox_key(project_dir: &Path, sandbox_name: &str) -> String {
    format!("{}:{}", project_dir.display(), sandbox_name)
}

/// Handle a single client connection in a dedicated thread.
fn handle_client(mut stream: UnixStream, state: SharedState, client_id: u64) {
    info!("Client {} connected", client_id);

    // Read the client request
    let request = match server::read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            error!("Client {}: failed to read request: {}", client_id, e);
            let _ = server::send_error(&mut stream, -32700, "Parse error");
            return;
        }
    };

    debug!("Client {}: received request: {:?}", client_id, request);

    match request {
        server::ClientRequest::EnsureSandbox {
            sandbox_name,
            params,
        } => {
            handle_ensure_sandbox(stream, state, client_id, &sandbox_name, params);
        }
    }
}

/// Handle an EnsureSandbox request from a client.
fn handle_ensure_sandbox(
    mut stream: UnixStream,
    state: SharedState,
    client_id: u64,
    sandbox_name: &str,
    params: SandboxParams,
) {
    let key = sandbox_key(&params.project_dir, sandbox_name);

    // Check if sandbox already exists or we need to create it
    let needs_creation = {
        let mut state = state.lock().unwrap();
        if let Some(sandbox_state) = state.sandboxes.get_mut(&key) {
            // Sandbox exists, increment client count
            sandbox_state.client_count += 1;
            info!(
                "Client {}: attached to existing sandbox '{}' (clients: {})",
                client_id, key, sandbox_state.client_count
            );
            false
        } else {
            true
        }
    };

    if needs_creation {
        // Create the sandbox
        info!("Client {}: creating sandbox '{}'", client_id, key);

        let user_info: UserInfo = params.user_info.clone().into();
        let runtime: Runtime = params.runtime.into();
        let overlay_mode: OverlayMode = params.overlay_mode.into();

        // Load sandbox config and ensure sandbox is set up
        let sandbox_config = match SandboxConfig::load(&params.project_dir) {
            Ok(c) => c,
            Err(e) => {
                error!("Client {}: failed to load sandbox config: {}", client_id, e);
                let _ = server::send_error(
                    &mut stream,
                    -32000,
                    &format!("Failed to load sandbox config: {}", e),
                );
                return;
            }
        };

        let info = match crate::sandbox::ensure_sandbox(
            &params.project_dir,
            sandbox_name,
            &sandbox_config,
        ) {
            Ok(i) => i,
            Err(e) => {
                error!("Client {}: failed to ensure sandbox: {}", client_id, e);
                let _ = server::send_error(
                    &mut stream,
                    -32000,
                    &format!("Failed to ensure sandbox: {}", e),
                );
                return;
            }
        };

        // Start the container
        if let Err(e) = start_container(
            &info,
            &params.image_tag,
            &user_info,
            runtime,
            overlay_mode,
            &params.env_vars,
        ) {
            error!("Client {}: failed to start container: {}", client_id, e);
            let _ = server::send_error(
                &mut stream,
                -32000,
                &format!("Failed to start container: {}", e),
            );
            return;
        }

        // Start git sync thread
        let git_sync = match GitSyncThread::spawn(info.clone()) {
            Ok(g) => g,
            Err(e) => {
                error!("Client {}: failed to start git sync: {}", client_id, e);
                // Container is started, so we should still proceed
                // but log the error - create a dummy git sync
                warn!("Proceeding without git sync");
                // We need to handle this case - for now, let's fail
                let _ = server::send_error(
                    &mut stream,
                    -32000,
                    &format!("Failed to start git sync: {}", e),
                );
                return;
            }
        };

        // Add to state
        {
            let mut state = state.lock().unwrap();
            state.sandboxes.insert(
                key.clone(),
                SandboxState {
                    info,
                    git_sync,
                    client_count: 1,
                },
            );
        }

        info!(
            "Client {}: sandbox '{}' created successfully",
            client_id, key
        );
    }

    // Send success response
    if let Err(e) = server::send_ensure_sandbox_ok(&mut stream) {
        error!("Client {}: failed to send response: {}", client_id, e);
        // Decrement client count since we failed
        let mut state = state.lock().unwrap();
        if let Some(sandbox_state) = state.sandboxes.get_mut(&key) {
            sandbox_state.client_count -= 1;
        }
        return;
    }

    // Wait for client to disconnect by blocking on read.
    // When the client closes its end, read() returns Ok(0) or an error.
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => info!("Client {} disconnected", client_id),
        Ok(_) => info!("Client {} sent unexpected data, disconnecting", client_id),
        Err(e) => info!("Client {} connection error: {}", client_id, e),
    }

    // Decrement client count and clean up if needed
    let mut state = state.lock().unwrap();
    if let Some(sandbox_state) = state.sandboxes.get_mut(&key) {
        sandbox_state.client_count -= 1;
        info!(
            "Client {}: sandbox '{}' now has {} clients",
            client_id, key, sandbox_state.client_count
        );

        if sandbox_state.client_count == 0 {
            info!("Sandbox '{}' has no clients, cleaning up", key);

            // Take ownership of the sandbox state to clean up
            if let Some(sandbox_state) = state.sandboxes.remove(&key) {
                // Stop git sync thread (runs final sync)
                sandbox_state.git_sync.stop();

                // Stop container
                if let Err(e) = docker::stop_container(&sandbox_state.info.container_name) {
                    error!("Failed to stop container: {}", e);
                }

                info!("Sandbox '{}' cleaned up", key);
            }
        }
    }
}

/// Run the daemon, listening for connections on the socket.
pub fn run_daemon() -> Result<()> {
    let sock_path = socket_path()?;

    info!("Daemon starting, socket: {}", sock_path.display());

    // Create parent directory if needed
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
    }

    // Remove existing socket if present
    if sock_path.exists() {
        std::fs::remove_file(&sock_path).with_context(|| {
            format!("Failed to remove existing socket: {}", sock_path.display())
        })?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("Failed to bind socket: {}", sock_path.display()))?;

    info!("Listening on {}", sock_path.display());

    let state = Arc::new(Mutex::new(DaemonState::new()));
    let mut client_id: u64 = 0;

    // The daemon runs forever, accepting connections.
    // It is expected to be terminated by a signal (e.g. SIGTERM).
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                client_id += 1;
                let state = Arc::clone(&state);
                let id = client_id;

                thread::spawn(move || {
                    handle_client(stream, state, id);
                });
            }
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }
    }
}
