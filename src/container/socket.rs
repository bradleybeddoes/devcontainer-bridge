//! Container-side Unix socket mirror management.
//!
//! When the host sends `SocketForward`, this module creates a `UnixListener`
//! at the specified container path. Client connections are forwarded back to
//! the host via `SocketConnectRequest` and the reverse data channel.

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixListener};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::container::RelayMessage;
use crate::protocol::Message;

/// Errors from socket mirror operations.
#[derive(Debug, Error)]
pub enum SocketMirrorError {
    /// Failed to create parent directories.
    #[error("failed to create parent directory {path}: {source}")]
    CreateDir {
        /// The directory path that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to bind the Unix listener.
    #[error("failed to bind socket at {path}: {source}")]
    Bind {
        /// The socket path that could not be bound.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to set socket permissions.
    #[error("failed to set permissions on {path}: {source}")]
    Permissions {
        /// The path whose permissions could not be set.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A mirror Unix socket listener in the container.
pub struct MirrorSocket {
    /// The socket forward identifier.
    pub socket_id: String,
    /// Path of the socket file in the container.
    pub container_path: PathBuf,
    /// The Unix listener (used by the accept loop).
    pub listener: Option<UnixListener>,
    /// Shutdown signal sender.
    pub shutdown_tx: watch::Sender<bool>,
    /// Shutdown signal receiver (clone for the accept loop).
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Creates a mirror Unix socket at the given container path.
///
/// Parent directories are created with mode `0o700` if they don't exist.
/// Any stale socket file at the path is removed before binding.
/// The new socket file is set to mode `0o600`.
///
/// # Arguments
///
/// * `socket_id` — Unique identifier for this socket forward.
/// * `container_path` — Absolute path where the socket should be created.
///
/// # Errors
///
/// Returns [`SocketMirrorError::CreateDir`] if parent directories cannot be created.
/// Returns [`SocketMirrorError::Bind`] if the Unix listener cannot be bound.
/// Returns [`SocketMirrorError::Permissions`] if the socket file permissions cannot be set.
pub fn create_mirror_socket(
    socket_id: &str,
    container_path: &Path,
) -> Result<MirrorSocket, SocketMirrorError> {
    // Create parent directories with mode 0700
    if let Some(parent) = container_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| SocketMirrorError::CreateDir {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Set parent directory permissions to 0700
        use std::os::unix::fs::PermissionsExt;
        let dir_perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(parent, dir_perms).map_err(|e| SocketMirrorError::Permissions {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    // Remove stale socket file if it exists (ignore errors — file may not exist)
    let _ = fs::remove_file(container_path);

    // Bind using std::os::unix::net::UnixListener, then convert to tokio.
    // The tokio conversion requires a runtime context, so callers must ensure
    // this function is called within a tokio runtime.
    let std_listener = std::os::unix::net::UnixListener::bind(container_path).map_err(|e| {
        SocketMirrorError::Bind {
            path: container_path.to_path_buf(),
            source: e,
        }
    })?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| SocketMirrorError::Bind {
            path: container_path.to_path_buf(),
            source: e,
        })?;
    let listener = UnixListener::from_std(std_listener).map_err(|e| SocketMirrorError::Bind {
        path: container_path.to_path_buf(),
        source: e,
    })?;

    // Set socket file permissions to 0600
    use std::os::unix::fs::PermissionsExt;
    let sock_perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(container_path, sock_perms).map_err(|e| {
        SocketMirrorError::Permissions {
            path: container_path.to_path_buf(),
            source: e,
        }
    })?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    info!(
        socket_id = %socket_id,
        path = %container_path.display(),
        "created mirror socket"
    );

    Ok(MirrorSocket {
        socket_id: socket_id.to_owned(),
        container_path: container_path.to_path_buf(),
        listener: Some(listener),
        shutdown_tx,
        shutdown_rx,
    })
}

/// Removes a mirror socket file and attempts to clean up empty parent directories.
///
/// The socket file is removed unconditionally (errors are logged but not propagated).
/// If the parent directory is empty after removal, it is also removed.
pub fn remove_mirror_socket(mirror: &MirrorSocket) {
    match fs::remove_file(&mirror.container_path) {
        Ok(()) => {
            debug!(
                socket_id = %mirror.socket_id,
                path = %mirror.container_path.display(),
                "removed mirror socket file"
            );
        }
        Err(e) => {
            warn!(
                socket_id = %mirror.socket_id,
                path = %mirror.container_path.display(),
                error = %e,
                "failed to remove mirror socket file"
            );
        }
    }

    // Try to remove parent directory if empty (ignore errors — may not be empty)
    if let Some(parent) = mirror.container_path.parent() {
        let _ = fs::remove_dir(parent);
    }

    info!(
        socket_id = %mirror.socket_id,
        path = %mirror.container_path.display(),
        "cleaned up mirror socket"
    );
}

/// Run the accept loop for a mirror socket.
///
/// For each client connection:
/// 1. Generate a `conn_id` (UUID)
/// 2. Send `SocketConnectRequest { socket_id, conn_id }` on the control channel
/// 3. Wait for the session loop to acknowledge the message was sent
/// 4. Open a reverse TCP connection to the host data port
/// 5. Send `ConnectReady { conn_id }` on the data connection
/// 6. Bridge the `UnixStream` (client) and `TcpStream` (data) bidirectionally
///
/// Runs until the shutdown signal is received.
///
/// # Arguments
///
/// * `socket_id` — The unique identifier for this socket forward.
/// * `listener` — The `UnixListener` accepting client connections.
/// * `control_tx` — Channel to send relay messages (e.g., `SocketConnectRequest`) back to the session loop.
/// * `host_data_addr` — The host data port address for reverse TCP connections.
/// * `shutdown_rx` — Watch channel that signals shutdown when set to `true`.
pub async fn run_mirror_accept_loop(
    socket_id: String,
    listener: UnixListener,
    control_tx: mpsc::Sender<RelayMessage>,
    host_data_addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((unix_stream, _addr)) => {
                        let conn_id = uuid::Uuid::new_v4().to_string();
                        let socket_id = socket_id.clone();
                        let control_tx = control_tx.clone();

                        tokio::spawn(async move {
                            handle_socket_client(
                                socket_id, conn_id, unix_stream,
                                control_tx, host_data_addr,
                            ).await;
                        });
                    }
                    Err(e) => {
                        warn!(socket_id = %socket_id, error = %e, "accept failed on mirror socket");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                debug!(socket_id = %socket_id, "mirror socket accept loop shutting down");
                break;
            }
        }
    }
}

/// Handle a single client connection to a mirror socket.
///
/// Sends `SocketConnectRequest` on the control channel, waits for the session
/// loop to acknowledge the send, then opens a reverse data connection to the
/// host, sends `ConnectReady`, and bridges the Unix stream and TCP stream
/// bidirectionally.
///
/// The ack-before-connect pattern ensures the host has received and registered
/// a pending connection for the `conn_id` before the data channel's
/// `ConnectReady` arrives, preventing a race where the host would discard the
/// data connection as unmatched.
async fn handle_socket_client(
    socket_id: String,
    conn_id: String,
    mut unix_stream: tokio::net::UnixStream,
    control_tx: mpsc::Sender<RelayMessage>,
    host_data_addr: SocketAddr,
) {
    debug!(%conn_id, %socket_id, "handling mirror socket client connection");

    // 1. Send SocketConnectRequest on control channel and wait for ack
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if control_tx
        .send(RelayMessage {
            msg: Message::SocketConnectRequest {
                socket_id: socket_id.clone(),
                conn_id: conn_id.clone(),
            },
            ack_tx: Some(ack_tx),
        })
        .await
        .is_err()
    {
        warn!(%conn_id, "control channel closed, cannot send SocketConnectRequest");
        return;
    }

    // Wait for the session loop to confirm the message was written to the
    // control connection. This ensures the host will have the pending
    // connection registered before our ConnectReady arrives.
    if ack_rx.await.is_err() {
        warn!(%conn_id, "session loop dropped ack sender before confirming SocketConnectRequest");
        return;
    }

    // 2. Connect to host data port (reverse TCP connection)
    let mut data_stream = match TcpStream::connect(host_data_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%conn_id, error = %e, "failed to connect to host data port");
            return;
        }
    };

    // 3. Send ConnectReady handshake on data connection
    let ready_msg = Message::ConnectReady {
        conn_id: conn_id.clone(),
    };
    let mut json = match serde_json::to_string(&ready_msg) {
        Ok(j) => j,
        Err(e) => {
            warn!(%conn_id, error = %e, "failed to serialize ConnectReady");
            return;
        }
    };
    json.push('\n');

    if let Err(e) = data_stream.write_all(json.as_bytes()).await {
        warn!(%conn_id, error = %e, "failed to send ConnectReady on data channel");
        return;
    }
    if let Err(e) = data_stream.flush().await {
        warn!(%conn_id, error = %e, "failed to flush ConnectReady on data channel");
        return;
    }

    info!(%conn_id, %socket_id, "socket data connection ready, starting bridge");

    // 4. Bridge UnixStream <-> TcpStream bidirectionally
    match tokio::io::copy_bidirectional(&mut unix_stream, &mut data_stream).await {
        Ok((c2h, h2c)) => {
            debug!(
                %conn_id,
                client_to_host = c2h,
                host_to_client = h2c,
                "socket bridge completed"
            );
        }
        Err(e) => {
            debug!(%conn_id, error = %e, "socket bridge ended");
        }
    }
}

/// Removes all mirror sockets and clears the map.
///
/// Each mirror socket is individually cleaned up via [`remove_mirror_socket`].
pub fn cleanup_all_mirrors(mirrors: &mut HashMap<String, MirrorSocket>) {
    for mirror in mirrors.values() {
        remove_mirror_socket(mirror);
    }
    mirrors.clear();
    info!("cleaned up all mirror sockets");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_mirror_socket_creates_file() {
        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("test.sock");

        let mirror = create_mirror_socket("sock-1", &sock_path).unwrap();
        assert!(
            sock_path.exists(),
            "socket file should exist after creation"
        );
        assert_eq!(mirror.socket_id, "sock-1");
        assert_eq!(mirror.container_path, sock_path);
        assert!(mirror.listener.is_some());
    }

    #[tokio::test]
    async fn test_create_mirror_socket_permissions() {
        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("perms.sock");

        let _mirror = create_mirror_socket("sock-2", &sock_path).unwrap();

        let metadata = fs::metadata(&sock_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        // On some systems the mode for sockets may include the socket type bits,
        // so we check only the permission bits we set.
        assert_eq!(
            mode & 0o600,
            0o600,
            "socket file should have at least 0600 permissions, got {mode:o}"
        );
    }

    #[tokio::test]
    async fn test_create_mirror_socket_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        let sock_path = nested.join("deep.sock");

        let _mirror = create_mirror_socket("sock-3", &sock_path).unwrap();

        assert!(
            nested.exists(),
            "nested parent directories should be created"
        );
        let dir_meta = fs::metadata(&nested).unwrap();
        let dir_mode = dir_meta.permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "parent directory should have 0700 permissions, got {dir_mode:o}"
        );
    }

    #[tokio::test]
    async fn test_create_mirror_socket_removes_stale() {
        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("stale.sock");

        // Create a regular file at the socket path (simulating a stale socket)
        fs::write(&sock_path, b"stale data").unwrap();
        assert!(sock_path.exists());

        // Creating a mirror socket should remove the stale file and succeed
        let mirror = create_mirror_socket("sock-4", &sock_path).unwrap();
        assert!(mirror.listener.is_some());

        // Verify we can connect to the socket (it's a real socket, not a regular file)
        let conn = std::os::unix::net::UnixStream::connect(&sock_path);
        assert!(
            conn.is_ok(),
            "should be able to connect to the mirror socket"
        );
    }

    #[tokio::test]
    async fn test_remove_mirror_socket_cleanup() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("removeme");
        let sock_path = subdir.join("cleanup.sock");

        let mirror = create_mirror_socket("sock-5", &sock_path).unwrap();
        assert!(sock_path.exists());
        assert!(subdir.exists());

        remove_mirror_socket(&mirror);

        assert!(
            !sock_path.exists(),
            "socket file should be removed after cleanup"
        );
        assert!(
            !subdir.exists(),
            "empty parent directory should be removed after cleanup"
        );
    }

    #[tokio::test]
    async fn test_multiple_mirror_sockets() {
        let tmp = TempDir::new().unwrap();

        let paths: Vec<PathBuf> = (0..3)
            .map(|i| tmp.path().join(format!("multi-{i}.sock")))
            .collect();

        let mirrors: Vec<MirrorSocket> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| create_mirror_socket(&format!("multi-{i}"), p).unwrap())
            .collect();

        // All socket files should exist simultaneously
        for path in &paths {
            assert!(path.exists(), "socket {} should exist", path.display());
        }

        assert_eq!(mirrors.len(), 3);
    }

    #[tokio::test]
    async fn test_cleanup_all_mirrors() {
        let tmp = TempDir::new().unwrap();

        let mut mirrors = HashMap::new();
        for i in 0..4 {
            let subdir = tmp.path().join(format!("dir-{i}"));
            let sock_path = subdir.join("sock");
            let mirror = create_mirror_socket(&format!("all-{i}"), &sock_path).unwrap();
            mirrors.insert(mirror.socket_id.clone(), mirror);
        }

        // All should exist
        assert_eq!(mirrors.len(), 4);

        cleanup_all_mirrors(&mut mirrors);

        assert!(mirrors.is_empty(), "map should be cleared after cleanup");

        // Verify all socket files and directories are gone
        for i in 0..4 {
            let subdir = tmp.path().join(format!("dir-{i}"));
            let sock_path = subdir.join("sock");
            assert!(
                !sock_path.exists(),
                "socket file dir-{i}/sock should be removed"
            );
            assert!(
                !subdir.exists(),
                "directory dir-{i} should be removed if empty"
            );
        }
    }
}
