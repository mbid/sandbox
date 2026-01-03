use anyhow::{bail, Context, Result};
use backoff::{backoff::Backoff, ExponentialBackoff};
use log::{debug, error, info};
use nix::fcntl::{Flock, FlockArg};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::File;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{OverlayMode, Runtime, UserInfo};
use crate::daemon_protocol::{self, DaemonApi, SandboxParams};
use crate::docker;
use crate::git;
use crate::sandbox::SandboxInfo;

const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

fn socket_path(info: &SandboxInfo) -> PathBuf {
    // Unix domain sockets have a 108-char path limit on Linux.
    // Use /tmp/sandbox/ with a hash to keep paths short.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(info.sandbox_dir.to_string_lossy().as_bytes());
    let hash = hex::encode(&hasher.finalize()[..8]);
    PathBuf::from(format!("/tmp/sandbox/{}.sock", hash))
}

fn lockfile_path(info: &SandboxInfo) -> PathBuf {
    info.sandbox_dir.join("daemon.lock")
}

/// A file lock that releases when dropped (wrapper around nix::fcntl::Flock).
type FileLock = Flock<File>;

fn try_acquire_lock(lock_path: &Path) -> Result<Option<FileLock>> {
    std::fs::create_dir_all(lock_path.parent().unwrap())?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, e)) => Err(e).context("Failed to acquire lock"),
    }
}

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

/// Handle to a daemon connection. Dropping this signals disconnection to the daemon.
pub struct DaemonConnection {
    // Hold the stream to keep the connection alive; the daemon shuts down
    // when all clients disconnect.
    #[allow(dead_code)]
    stream: UnixStream,
}

pub fn connect_or_launch(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<DaemonConnection> {
    let sock_path = socket_path(info);
    let lock_path = lockfile_path(info);

    let params = SandboxParams {
        image_tag: image_tag.to_string(),
        user_info: user_info.into(),
        runtime: runtime.into(),
        overlay_mode: overlay_mode.into(),
        env_vars: env_vars.to_vec(),
    };

    // Try to connect to existing socket
    // TODO: We should verify that the existing sandbox was launched with the same
    // parameters, and return an error if they differ. Currently we silently ignore
    // any parameter differences.
    if let Ok(stream) = UnixStream::connect(&sock_path) {
        debug!("Connected to existing daemon");
        let stream = do_handshake(stream, &info.name, &params)?;
        return Ok(DaemonConnection { stream });
    }

    // Spawn a new daemon (it will handle lock acquisition with backoff)
    debug!("Launching daemon...");
    spawn_daemon(info)?;

    // Wait for socket to become connectable (not just exist)
    let stream = wait_for_socket_connectable(&sock_path, &lock_path)?;
    let stream = do_handshake(stream, &info.name, &params)?;

    Ok(DaemonConnection { stream })
}

fn do_handshake(
    stream: UnixStream,
    sandbox_name: &str,
    params: &SandboxParams,
) -> Result<UnixStream> {
    let mut client = daemon_protocol::Client::new(stream);
    client.ensure_sandbox(sandbox_name, params)?;
    Ok(client.into_inner())
}

fn spawn_daemon(info: &SandboxInfo) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    let mut cmd = Command::new(exe);
    cmd.arg("internal-daemon").arg(&info.sandbox_dir);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Start daemon in its own process group so it survives if the parent
        // process is killed (e.g., when a PTY session terminates).
        .process_group(0)
        .spawn()
        .context("Failed to spawn daemon process")?;

    Ok(())
}

/// Wait for a socket to become connectable. This handles the case where another daemon
/// is shutting down (holding the lock) and a new daemon will take over.
fn wait_for_socket_connectable(sock_path: &Path, lock_path: &Path) -> Result<UnixStream> {
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!("Timeout waiting for daemon socket to become connectable");
        }

        // Try to connect
        if sock_path.exists() {
            if let Ok(stream) = UnixStream::connect(sock_path) {
                return Ok(stream);
            }
            // Socket exists but not connectable - maybe orphaned.
            // Try to clean it up if we can get the lock.
            if let Ok(Some(lock)) = try_acquire_lock(lock_path) {
                debug!("Acquired lock while waiting, removing orphaned socket");
                let _ = std::fs::remove_file(sock_path);
                drop(lock);
            }
        }

        std::thread::sleep(poll_interval.min(remaining));
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

fn run_git_sync_thread(info: SandboxInfo) -> Result<()> {
    let debounce = Duration::from_millis(500);
    let mut last_sync = Instant::now();
    let mut pending_sync = false;

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
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
        match rx.recv_timeout(Duration::from_millis(100)) {
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

fn acquire_lock_with_backoff(lock_path: &Path) -> Result<FileLock> {
    let mut backoff = ExponentialBackoff {
        initial_interval: Duration::from_millis(10),
        max_interval: Duration::from_secs(1),
        max_elapsed_time: Some(Duration::from_secs(30)),
        ..ExponentialBackoff::default()
    };

    loop {
        match try_acquire_lock(lock_path)? {
            Some(lock) => return Ok(lock),
            None => match backoff.next_backoff() {
                Some(duration) => {
                    info!("Lock held by another daemon, retrying in {:?}", duration);
                    std::thread::sleep(duration);
                }
                None => {
                    bail!("Timeout acquiring daemon lock after 30 seconds");
                }
            },
        }
    }
}

fn bind_socket(sock_path: &Path) -> Result<UnixListener> {
    let scratch_dir = Path::new("/tmp/sandbox/scratch");
    std::fs::create_dir_all(scratch_dir).context("Failed to create scratch directory")?;

    let sock_parent = sock_path.parent().unwrap();
    std::fs::create_dir_all(sock_parent).with_context(|| {
        format!(
            "Failed to create socket directory: {}",
            sock_parent.display()
        )
    })?;

    let random_id: u64 = rand::random();
    let temp_path = scratch_dir.join(format!("{:016x}.sock", random_id));

    let listener = UnixListener::bind(&temp_path)
        .with_context(|| format!("Failed to bind socket at {}", temp_path.display()))?;

    // Remove any existing socket (must be orphaned since we hold the lock)
    let _ = std::fs::remove_file(sock_path);

    // Move socket to final location
    std::fs::rename(&temp_path, sock_path).with_context(|| {
        format!(
            "Failed to move socket from {} to {}",
            temp_path.display(),
            sock_path.display()
        )
    })?;

    Ok(listener)
}

/// Result of reading a client request.
struct ClientRequestResult {
    stream: UnixStream,
    params: SandboxParams,
}

/// Read a client request and extract sandbox parameters.
/// Returns None on error (after sending error response).
fn read_client_request(mut stream: UnixStream) -> Option<ClientRequestResult> {
    use daemon_protocol::server::{self, ClientRequest};

    let request = match server::read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to read client request: {}", e);
            let _ = server::send_error(&mut stream, -32700, "Parse error");
            return None;
        }
    };

    debug!("Received request: {:?}", request);

    match request {
        ClientRequest::EnsureSandbox { params, .. } => Some(ClientRequestResult { stream, params }),
    }
}

/// Send success response to a client and prepare the stream for connection tracking.
/// Returns the stream on success, or None on error.
fn send_success_and_track(mut stream: UnixStream) -> Option<UnixStream> {
    use daemon_protocol::server;

    if server::send_ensure_sandbox_ok(&mut stream).is_err() {
        error!("Failed to send response to client");
        return None;
    }

    stream.set_nonblocking(true).ok();
    Some(stream)
}

pub fn run_daemon(info: &SandboxInfo) -> Result<()> {
    info!("Daemon starting for sandbox '{}'", info.name);

    let lock_path = lockfile_path(info);
    let sock_path = socket_path(info);

    let lock = match acquire_lock_with_backoff(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to acquire lock: {}", e);
            return Err(e);
        }
    };

    info!("Acquired daemon lock");

    let listener = match bind_socket(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind socket: {}", e);
            drop(lock);
            return Err(e);
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        error!("Failed to set socket non-blocking: {}", e);
        drop(lock);
        return Err(e.into());
    }

    info!("Listening on {}", sock_path.display());

    let mut clients: Vec<UnixStream> = Vec::new();
    // Pending clients: streams that have been read but are waiting for container to start
    let mut pending_clients: Vec<UnixStream> = Vec::new();
    let start = Instant::now();
    let mut container_started = false;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                info!(
                    "Client connected (total: {})",
                    clients.len() + pending_clients.len() + 1
                );

                if container_started {
                    // Container already running - just read request and respond
                    if let Some(req) = read_client_request(stream) {
                        if let Some(stream) = send_success_and_track(req.stream) {
                            clients.push(stream);
                        }
                    }
                } else {
                    // Container not started yet - read request to get params
                    let Some(req) = read_client_request(stream) else {
                        continue;
                    };

                    let is_first = pending_clients.is_empty();
                    pending_clients.push(req.stream);

                    if is_first {
                        // First client provides the sandbox parameters
                        let params = req.params;
                        let user_info: UserInfo = params.user_info.into();
                        let runtime: Runtime = params.runtime.into();
                        let overlay_mode: OverlayMode = params.overlay_mode.into();

                        info!("First client connected, starting container...");
                        match start_container(
                            info,
                            &params.image_tag,
                            &user_info,
                            runtime,
                            overlay_mode,
                            &params.env_vars,
                        ) {
                            Ok(()) => {
                                container_started = true;
                                info!("Container started successfully");

                                for client in pending_clients.drain(..) {
                                    if let Some(stream) = send_success_and_track(client) {
                                        clients.push(stream);
                                    }
                                }

                                let sync_info = info.clone();
                                thread::spawn(move || {
                                    if let Err(e) = run_git_sync_thread(sync_info) {
                                        error!("Git sync thread failed: {:#}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Failed to start container: {}", e);
                                drop(listener);
                                drop(lock);
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }

        clients.retain_mut(|stream| {
            let mut buf = [0u8; 1];
            match stream.read(&mut buf) {
                Ok(0) => false,
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(_) => false,
            }
        });

        if container_started && clients.is_empty() && pending_clients.is_empty() {
            info!("All clients disconnected, shutting down...");
            break;
        }

        if !container_started
            && clients.is_empty()
            && pending_clients.is_empty()
            && start.elapsed() > FIRST_CLIENT_TIMEOUT
        {
            info!("No clients connected within timeout, shutting down...");
            drop(listener);
            drop(lock);
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    drop(listener);

    info!("Running final sync before shutdown...");
    if let Err(e) = run_full_git_sync(info) {
        error!("Final git sync failed: {:#}", e);
    }

    info!("Stopping container...");
    if let Err(e) = docker::stop_container(&info.container_name) {
        error!("Error stopping container: {}", e);
    }

    drop(lock);

    info!("Daemon exiting");
    Ok(())
}
