use anyhow::{bail, Context, Result};
use log::debug;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::{OverlayMode, Runtime, UserInfo};
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

fn bind_socket(sock_path: &Path, log_file: &mut std::fs::File) -> Result<UnixListener> {
    let temp_path = sock_path.with_extension("sock.tmp");

    // Create parent directory if needed
    if let Some(parent) = temp_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create socket directory: {}", parent.display())
            })?;
        }
    }

    // Clean up any stale temp socket
    let _ = std::fs::remove_file(&temp_path);

    let listener = UnixListener::bind(&temp_path).with_context(|| {
        format!(
            "Failed to bind socket at {} (errno: {:?})",
            temp_path.display(),
            std::io::Error::last_os_error()
        )
    })?;

    // Atomically publish the socket via hard link
    match std::fs::hard_link(&temp_path, sock_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temp_path);
            Ok(listener)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another daemon was faster
            let _ = std::fs::remove_file(&temp_path);
            log(log_file, "Another daemon already running, exiting");
            bail!("Another daemon is already running");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(e).context("Failed to publish socket")
        }
    }
}

fn cleanup_socket(sock_path: &Path) {
    // TODO: Use flock for truly graceful cleanup
    let _ = std::fs::remove_file(sock_path);
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

pub struct DaemonConnection {
    stream: UnixStream,
}

impl DaemonConnection {
    pub fn check_alive(&mut self) -> bool {
        let mut buf = [0u8; 1];
        match self.stream.read(&mut buf) {
            Ok(0) => false, // EOF - daemon exited
            Ok(_) => true,  // Got data (unexpected, but daemon is alive)
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false, // Error - assume dead
        }
    }

    fn wait_for_ready(&mut self) -> Result<()> {
        self.stream.set_nonblocking(false)?;
        let mut buf = [0u8; 1];
        match self.stream.read_exact(&mut buf) {
            Ok(()) => {
                self.stream.set_nonblocking(true)?;
                Ok(())
            }
            Err(e) => {
                bail!("Daemon failed to start container: {}", e);
            }
        }
    }
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

    if sock_path.exists() {
        match UnixStream::connect(&sock_path) {
            Ok(stream) => {
                debug!("Connected to existing daemon");
                let mut conn = DaemonConnection { stream };
                match conn.wait_for_ready() {
                    Ok(()) => return Ok(conn),
                    Err(_) => {
                        // Connected but daemon is shutting down - wait for it to finish
                        debug!("Daemon is shutting down, waiting for socket to disappear...");
                        wait_for_socket_removal(&sock_path)?;
                    }
                }
            }
            Err(_) => {
                // Socket exists but can't connect - daemon may be shutting down
                debug!("Cannot connect to daemon, waiting for socket to disappear...");
                wait_for_socket_removal(&sock_path)?;
            }
        }
    }

    debug!("Launching daemon...");
    spawn_daemon(info, image_tag, user_info, runtime, overlay_mode, env_vars)?;

    debug!("Waiting for daemon socket to appear...");
    wait_for_socket(&sock_path)?;

    let stream =
        UnixStream::connect(&sock_path).context("Failed to connect to daemon after launch")?;
    let mut conn = DaemonConnection { stream };
    conn.wait_for_ready()?;

    Ok(conn)
}

fn spawn_daemon(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    let mut cmd = Command::new(exe);
    cmd.arg("internal-daemon")
        .arg(&info.sandbox_dir)
        .arg(image_tag)
        .arg(&user_info.username)
        .arg(user_info.uid.to_string())
        .arg(user_info.gid.to_string())
        .arg(&user_info.shell)
        .arg(runtime.docker_runtime_name())
        .arg(match overlay_mode {
            OverlayMode::Overlayfs => "overlayfs",
            OverlayMode::Copy => "copy",
        });

    for (name, value) in env_vars {
        cmd.arg(format!("{}={}", name, value));
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn daemon process")?;

    Ok(())
}

fn wait_for_socket_removal(sock_path: &Path) -> Result<()> {
    // Check if already gone
    if !sock_path.exists() {
        return Ok(());
    }

    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    let parent = sock_path
        .parent()
        .context("Socket path has no parent directory")?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    watcher.watch(parent, RecursiveMode::NonRecursive)?;

    // Check again after setting up watcher
    if !sock_path.exists() {
        return Ok(());
    }

    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!("Timeout waiting for daemon to shut down");
        }

        match rx.recv_timeout(remaining) {
            Ok(Ok(_event)) => {
                if !sock_path.exists() {
                    return Ok(());
                }
            }
            Ok(Err(e)) => {
                bail!("File watcher error: {}", e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("Timeout waiting for daemon to shut down");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("File watcher disconnected");
            }
        }
    }
}

fn wait_for_socket(sock_path: &Path) -> Result<()> {
    // Check if already exists
    if sock_path.exists() {
        return Ok(());
    }

    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    // Watch the parent directory for the socket file to appear
    let parent = sock_path
        .parent()
        .context("Socket path has no parent directory")?;

    // Create parent directory if it doesn't exist (daemon might not have created it yet)
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    watcher.watch(parent, RecursiveMode::NonRecursive)?;

    // Check again after setting up watcher (in case file appeared between check and watch)
    if sock_path.exists() {
        return Ok(());
    }

    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!("Timeout waiting for daemon socket");
        }

        match rx.recv_timeout(remaining) {
            Ok(Ok(_event)) => {
                if sock_path.exists() {
                    return Ok(());
                }
            }
            Ok(Err(e)) => {
                bail!("File watcher error: {}", e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("Timeout waiting for daemon socket");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("File watcher disconnected");
            }
        }
    }
}

fn log(file: &mut std::fs::File, message: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "[{}] {}", timestamp, message);
}

pub fn run_daemon_with_sync(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<()> {
    let log_path = info.sandbox_dir.join("daemon.log");
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;

    log(
        &mut log_file,
        &format!("Daemon starting for sandbox '{}'", info.name),
    );

    let sock_path = socket_path(info);

    let listener = match bind_socket(&sock_path, &mut log_file) {
        Ok(l) => l,
        Err(e) => {
            log(&mut log_file, &format!("Failed to bind socket: {}", e));
            return Err(e);
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        log(
            &mut log_file,
            &format!("Failed to set socket non-blocking: {}", e),
        );
        return Err(e.into());
    }

    log(
        &mut log_file,
        &format!("Listening on {}", sock_path.display()),
    );

    let mut clients: Vec<UnixStream> = Vec::new();
    let mut pending_clients: Vec<UnixStream> = Vec::new();
    let start = Instant::now();
    let mut container_started = false;

    // Git sync state
    let (tx, rx) = mpsc::channel();
    // Watcher is stored here to keep it alive for the duration of the loop
    let mut _watcher: Option<RecommendedWatcher> = None;
    let debounce = Duration::from_millis(500);
    let main_sync_interval = Duration::from_secs(30);
    let mut last_sync = Instant::now();
    let mut last_main_sync = Instant::now();
    let mut pending_sync = false;

    loop {
        // Accept new connections
        match listener.accept() {
            Ok((mut stream, _)) => {
                log(
                    &mut log_file,
                    &format!(
                        "Client connected (total: {})",
                        clients.len() + pending_clients.len() + 1
                    ),
                );

                if container_started {
                    // Container is already running, send ready signal immediately
                    if stream.write_all(&[0u8]).is_ok() {
                        stream.set_nonblocking(true).ok();
                        clients.push(stream);
                    }
                } else {
                    // Queue client until container is ready
                    let is_first = pending_clients.is_empty();
                    pending_clients.push(stream);

                    if is_first {
                        log(
                            &mut log_file,
                            "First client connected, starting container...",
                        );
                        match start_container(
                            info,
                            image_tag,
                            user_info,
                            runtime,
                            overlay_mode,
                            env_vars,
                        ) {
                            Ok(()) => {
                                container_started = true;
                                log(&mut log_file, "Container started successfully");

                                // Send ready signal to all pending clients
                                for mut client in pending_clients.drain(..) {
                                    if client.write_all(&[0u8]).is_ok() {
                                        client.set_nonblocking(true).ok();
                                        clients.push(client);
                                    }
                                }

                                // Start file watcher for git sync
                                let tx_clone = tx.clone();
                                let mut watcher = RecommendedWatcher::new(
                                    move |res| {
                                        let _ = tx_clone.send(res);
                                    },
                                    Config::default().with_poll_interval(Duration::from_secs(1)),
                                )?;

                                let sandbox_git = info.clone_dir.join(".git");
                                if sandbox_git.exists() {
                                    watcher.watch(&sandbox_git, RecursiveMode::Recursive)?;
                                    log(
                                        &mut log_file,
                                        &format!("Watching: {}", sandbox_git.display()),
                                    );
                                }
                                _watcher = Some(watcher);
                            }
                            Err(e) => {
                                log(&mut log_file, &format!("Failed to start container: {}", e));
                                cleanup_socket(&sock_path);
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log(&mut log_file, &format!("Accept error: {}", e));
            }
        }

        // Check for disconnected clients
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
            log(&mut log_file, "All clients disconnected, shutting down...");
            break;
        }

        if !container_started
            && clients.is_empty()
            && pending_clients.is_empty()
            && start.elapsed() > FIRST_CLIENT_TIMEOUT
        {
            log(
                &mut log_file,
                "No clients connected within timeout, shutting down...",
            );
            cleanup_socket(&sock_path);
            return Ok(());
        }

        // Process git sync events
        if container_started {
            match rx.try_recv() {
                Ok(Ok(event)) => {
                    if !event.kind.is_access() {
                        pending_sync = true;
                    }
                }
                Ok(Err(e)) => {
                    log(&mut log_file, &format!("Watcher error: {}", e));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    log(&mut log_file, "Watcher channel disconnected");
                }
            }

            let now = Instant::now();

            // Sync sandbox changes to host
            if pending_sync && now.duration_since(last_sync) > debounce {
                if let Err(e) =
                    git::sync_sandbox_to_meta(&info.meta_git_dir, &info.clone_dir, &info.name)
                {
                    log(
                        &mut log_file,
                        &format!("Error syncing sandbox to meta.git: {}", e),
                    );
                } else if let Err(e) =
                    git::sync_meta_to_host(&info.repo_root, &info.meta_git_dir, &info.name)
                {
                    log(
                        &mut log_file,
                        &format!("Error syncing meta.git to host: {}", e),
                    );
                }
                last_sync = now;
                pending_sync = false;
            }

            // Periodically sync main branch from host
            if now.duration_since(last_main_sync) > main_sync_interval {
                if let Err(e) = git::sync_main_to_meta(&info.repo_root, &info.meta_git_dir) {
                    log(&mut log_file, &format!("Error syncing main branch: {}", e));
                }
                last_main_sync = now;
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    // Final sync before shutdown
    log(&mut log_file, "Running final sync before shutdown...");
    if let Err(e) = git::sync_sandbox_to_meta(&info.meta_git_dir, &info.clone_dir, &info.name) {
        log(
            &mut log_file,
            &format!("Error in final sandbox sync: {}", e),
        );
    } else if let Err(e) = git::sync_meta_to_host(&info.repo_root, &info.meta_git_dir, &info.name) {
        log(
            &mut log_file,
            &format!("Error in final meta-to-host sync: {}", e),
        );
    }

    log(&mut log_file, "Stopping container...");
    if let Err(e) = docker::stop_container(&info.container_name) {
        log(&mut log_file, &format!("Error stopping container: {}", e));
    }

    cleanup_socket(&sock_path);
    log(&mut log_file, "Daemon exiting");
    Ok(())
}
