use anyhow::{bail, Context, Result};
use log::debug;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempPath;

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

    let _temp_path = TempPath::from_path(&temp_path);

    // Atomically publish the socket via hard link
    match std::fs::hard_link(&temp_path, sock_path) {
        Ok(()) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            log(log_file, "Another daemon already running, exiting");
            Err(e).context("Another daemon is already running")
        }
        Err(e) => Err(e).context("Failed to publish socket"),
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
        let stream = UnixStream::connect(&sock_path).with_context(|| {
            format!(
                "Socket exists at {} but cannot connect",
                sock_path.display()
            )
        })?;
        debug!("Connected to existing daemon");
        let mut conn = DaemonConnection { stream };
        conn.wait_for_ready()?;
        return Ok(conn);
    }

    debug!("Launching daemon...");
    spawn_daemon(info, image_tag, user_info, runtime, overlay_mode, env_vars)?;

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

fn wait_for_socket(sock_path: &Path) -> Result<()> {
    if sock_path.exists() {
        return Ok(());
    }

    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    let parent = sock_path
        .parent()
        .context("Socket path has no parent directory")?;

    // Daemon might not have created it yet
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

    // File may have appeared between initial check and watch setup
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

fn run_git_sync_thread(info: SandboxInfo, log_path: PathBuf) {
    let mut log_file = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let debounce = Duration::from_millis(500);
    let main_sync_interval = Duration::from_secs(30);
    let mut last_sync = Instant::now();
    let mut last_main_sync = Instant::now();
    let mut pending_sync = false;

    let (tx, rx) = mpsc::channel();
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default().with_poll_interval(Duration::from_secs(1)),
    ) {
        Ok(w) => w,
        Err(e) => {
            log(&mut log_file, &format!("Failed to create watcher: {}", e));
            return;
        }
    };

    let sandbox_git = info.clone_dir.join(".git");
    if sandbox_git.exists() {
        if let Err(e) = watcher.watch(&sandbox_git, RecursiveMode::Recursive) {
            log(
                &mut log_file,
                &format!("Failed to watch {}: {}", sandbox_git.display(), e),
            );
            return;
        }
        log(
            &mut log_file,
            &format!("Git sync thread watching: {}", sandbox_git.display()),
        );
    } else {
        log(
            &mut log_file,
            &format!("No .git directory found at {}", sandbox_git.display()),
        );
        return;
    }

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                if !event.kind.is_access() {
                    pending_sync = true;
                }
            }
            Ok(Err(e)) => {
                log(&mut log_file, &format!("Watcher error: {}", e));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log(&mut log_file, "Git sync watcher channel disconnected");
                return;
            }
        }

        let now = Instant::now();

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

        if now.duration_since(last_main_sync) > main_sync_interval {
            if let Err(e) = git::sync_main_to_meta(&info.repo_root, &info.meta_git_dir) {
                log(&mut log_file, &format!("Error syncing main branch: {}", e));
            }
            last_main_sync = now;
        }
    }
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

    loop {
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
                    if stream.write_all(&[0u8]).is_ok() {
                        stream.set_nonblocking(true).ok();
                        clients.push(stream);
                    }
                } else {
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

                                for mut client in pending_clients.drain(..) {
                                    if client.write_all(&[0u8]).is_ok() {
                                        client.set_nonblocking(true).ok();
                                        clients.push(client);
                                    }
                                }

                                // Spawn git sync thread
                                let sync_info = info.clone();
                                let sync_log_path = log_path.clone();
                                thread::spawn(move || {
                                    run_git_sync_thread(sync_info, sync_log_path);
                                });
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

        std::thread::sleep(Duration::from_millis(100));
    }

    // Remove socket immediately so new clients can start a fresh daemon
    cleanup_socket(&sock_path);

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

    log(&mut log_file, "Daemon exiting");
    Ok(())
}
