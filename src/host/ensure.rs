//! Idempotent host daemon startup logic.
//!
//! `dbr ensure` checks if a host daemon is already running and starts one
//! if not. Used by shell aliases like `dcup` to guarantee the host daemon
//! is available before launching a devcontainer.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, info};

use crate::control;
use crate::protocol::Message;

/// Maximum time to wait for a newly spawned daemon to become ready.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between readiness checks after spawning.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Errors that can occur during the ensure operation.
#[derive(Debug, Error)]
pub enum EnsureError {
    /// The control port is in use by a process that is not a dbr host daemon.
    #[error(
        "port {port} is in use by another process (connected but did not respond with Pong). \
         Use `--control-port` to specify an alternative, and set `DCBRIDGE_HOST_PORT` in \
         your container environment to match."
    )]
    PortConflict {
        /// The port that is occupied.
        port: u16,
    },

    /// Failed to spawn the host daemon process.
    #[error("failed to spawn host daemon: {0}")]
    SpawnFailed(String),

    /// The spawned daemon did not become ready within the timeout.
    #[error(
        "host daemon did not become ready within {timeout_secs}s after spawning. \
         Check logs for errors."
    )]
    SpawnTimeout {
        /// How long we waited.
        timeout_secs: u64,
    },

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the `ensure` subcommand: start the host daemon if not already running.
///
/// 1. Try connecting to `host:control_port`
/// 2. If connection succeeds → send `Ping`, verify `Pong`, exit OK
/// 3. If connection refused → spawn host daemon as background process,
///    wait for port to become available, exit OK
/// 4. Write PID file to `~/.config/dbr/daemon.pid`
/// 5. If port in use by non-dbr process → fail with actionable error
///
/// # Race conditions
///
/// Between spawning the daemon and the first readiness check, another
/// process could bind the control port. The Ping/Pong verification
/// mitigates this — a non-dbr service will not respond with `Pong`,
/// causing the check to fail with [`EnsureError::PortConflict`].
///
/// # Errors
///
/// Returns [`EnsureError`] if the daemon cannot be started or verified.
pub async fn run_ensure(
    host: IpAddr,
    control_port: u16,
    data_port: u16,
) -> Result<(), EnsureError> {
    let addr: SocketAddr = (host, control_port).into();

    // Step 1: Try connecting
    if let Ok(mut conn) = control::connect(addr).await {
        if conn.send(&Message::Ping).await.is_err() {
            return Err(EnsureError::PortConflict { port: control_port });
        }
        match tokio::time::timeout(Duration::from_secs(3), conn.recv()).await {
            Ok(Ok(Message::Pong)) => {
                info!(port = control_port, "host daemon already running");
                println!("Host daemon already running on port {control_port}.");
                return Ok(());
            }
            _ => return Err(EnsureError::PortConflict { port: control_port }),
        }
    }
    debug!(port = control_port, "no daemon listening, spawning");

    // Step 2: Spawn the host daemon as a background process
    let exe = std::env::current_exe().map_err(|e| EnsureError::SpawnFailed(e.to_string()))?;

    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("host-daemon")
        .arg("--control-port")
        .arg(control_port.to_string())
        .arg("--data-port")
        .arg(data_port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| EnsureError::SpawnFailed(format!("{exe:?}: {e}")))?;

    let pid = child.id().unwrap_or(0);
    info!(pid, "spawned host daemon process");

    // Step 3: Write PID file
    if let Err(e) = write_pid_file(pid) {
        debug!(error = %e, "could not write PID file (non-fatal)");
    }

    // Step 4: Wait for daemon to become ready
    let deadline = tokio::time::Instant::now() + SPAWN_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(EnsureError::SpawnTimeout {
                timeout_secs: SPAWN_TIMEOUT.as_secs(),
            });
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        if let Ok(mut conn) = control::connect(addr).await {
            if conn.send(&Message::Ping).await.is_ok() {
                if let Ok(Ok(Message::Pong)) =
                    tokio::time::timeout(Duration::from_secs(2), conn.recv()).await
                {
                    println!("Host daemon started on port {control_port} (PID {pid}).");
                    return Ok(());
                }
            }
        }
    }
}

/// Write the daemon PID to `~/.config/dbr/daemon.pid`.
fn write_pid_file(pid: u32) -> Result<(), std::io::Error> {
    let path = pid_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, pid.to_string())?;
    debug!(path = %path.display(), pid, "wrote PID file");
    Ok(())
}

/// Return the path to the PID file: `~/.config/dbr/daemon.pid`.
fn pid_file_path() -> Result<PathBuf, std::io::Error> {
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("dbr")
        .join("daemon.pid"))
}
