use anyhow::{bail, Context, Result};
use backoff::{backoff::Backoff, ExponentialBackoff};
use log::{debug, error, info};
use nix::fcntl::{flock, FlockArg};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
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

fn lockfile_path(info: &SandboxInfo) -> PathBuf {
    info.sandbox_dir.join("daemon.lock")
}

/// A file lock that releases when dropped.
struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = flock(self.file.as_raw_fd(), FlockArg::Unlock);
    }
}

fn try_acquire_lock(lock_path: &Path) -> Result<Option<FileLock>> {
    std::fs::create_dir_all(lock_path.parent().unwrap())?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;

    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(Some(FileLock { file })),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
        Err(e) => Err(e).context("Failed to acquire lock"),
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
    let lock_path = lockfile_path(info);

    // Try to connect to existing socket
    if sock_path.exists() {
        match UnixStream::connect(&sock_path) {
            Ok(stream) => {
                debug!("Connected to existing daemon");
                let mut conn = DaemonConnection { stream };
                conn.wait_for_ready()?;
                return Ok(conn);
            }
            Err(_) => {}
        }
    }

    // Spawn a new daemon (it will handle lock acquisition with backoff)
    debug!("Launching daemon...");
    spawn_daemon(info, image_tag, user_info, runtime, overlay_mode, env_vars)?;

    // Wait for socket to become connectable (not just exist)
    let stream = wait_for_socket_connectable(&sock_path, &lock_path)?;
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

fn run_git_sync_thread(info: SandboxInfo) {
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
            error!("Failed to create watcher: {}", e);
            return;
        }
    };

    let sandbox_git = info.clone_dir.join(".git");
    if sandbox_git.exists() {
        if let Err(e) = watcher.watch(&sandbox_git, RecursiveMode::Recursive) {
            error!("Failed to watch {}: {}", sandbox_git.display(), e);
            return;
        }
        info!("Git sync thread watching: {}", sandbox_git.display());
    } else {
        info!("No .git directory found at {}", sandbox_git.display());
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
                error!("Watcher error: {}", e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Git sync watcher channel disconnected");
                return;
            }
        }

        let now = Instant::now();

        if pending_sync && now.duration_since(last_sync) > debounce {
            if let Err(e) =
                git::sync_sandbox_to_meta(&info.meta_git_dir, &info.clone_dir, &info.name)
            {
                error!("Error syncing sandbox to meta.git: {}", e);
            } else if let Err(e) =
                git::sync_meta_to_host(&info.repo_root, &info.meta_git_dir, &info.name)
            {
                error!("Error syncing meta.git to host: {}", e);
            }
            last_sync = now;
            pending_sync = false;
        }

        if now.duration_since(last_main_sync) > main_sync_interval {
            if let Err(e) = git::sync_main_to_meta(&info.repo_root, &info.meta_git_dir) {
                error!("Error syncing main branch: {}", e);
            }
            last_main_sync = now;
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

pub fn run_daemon_with_sync(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
) -> Result<()> {
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
    let mut pending_clients: Vec<UnixStream> = Vec::new();
    let start = Instant::now();
    let mut container_started = false;

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                info!(
                    "Client connected (total: {})",
                    clients.len() + pending_clients.len() + 1
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
                        info!("First client connected, starting container...");
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
                                info!("Container started successfully");

                                for mut client in pending_clients.drain(..) {
                                    if client.write_all(&[0u8]).is_ok() {
                                        client.set_nonblocking(true).ok();
                                        clients.push(client);
                                    }
                                }

                                let sync_info = info.clone();
                                thread::spawn(move || {
                                    run_git_sync_thread(sync_info);
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
    if let Err(e) = git::sync_sandbox_to_meta(&info.meta_git_dir, &info.clone_dir, &info.name) {
        error!("Error in final sandbox sync: {}", e);
    } else if let Err(e) = git::sync_meta_to_host(&info.repo_root, &info.meta_git_dir, &info.name) {
        error!("Error in final meta-to-host sync: {}", e);
    }

    info!("Stopping container...");
    if let Err(e) = docker::stop_container(&info.container_name) {
        error!("Error stopping container: {}", e);
    }

    drop(lock);

    info!("Daemon exiting");
    Ok(())
}
