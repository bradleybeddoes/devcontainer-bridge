//! Bridges client connections with reverse data connections from the container.
//!
//! When a client connects to a forwarded port, the host daemon sends a
//! `ConnectRequest` to the container. The container opens a reverse data
//! connection to the host data port and sends `ConnectReady`. This module
//! matches the two sides by `conn_id` and bridges them via
//! `tokio::io::copy_bidirectional`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, info, warn};

/// Timeout for waiting for a [`ConnectReady`] after sending [`ConnectRequest`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur during proxying.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// The container did not respond with `ConnectReady` in time.
    #[error("connect timeout for conn_id {conn_id} after {timeout_secs}s")]
    ConnectTimeout {
        /// The connection identifier that timed out.
        conn_id: String,
        /// The timeout duration in seconds.
        timeout_secs: u64,
    },

    /// The container reported a connection failure.
    #[error("container connect failed for conn_id {conn_id}: {error}")]
    ConnectFailed {
        /// The connection identifier.
        conn_id: String,
        /// Error message from the container.
        error: String,
    },

    /// An I/O error during bidirectional copy.
    #[error("proxy I/O error for conn_id {conn_id}: {source}")]
    Io {
        /// The connection identifier.
        conn_id: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A data connection with any data that was pre-read during the handshake.
///
/// When reading the `ConnectReady` message from the data port, the `BufReader`
/// may buffer data beyond the handshake line (e.g., data from the container's
/// local service that arrived in the same TCP segment). This struct carries
/// that pre-read data so it can be written to the client before starting the
/// bidirectional bridge.
pub struct DataStream {
    /// The raw TCP stream (after the ConnectReady line has been consumed).
    pub stream: TcpStream,
    /// Data that was buffered beyond the ConnectReady handshake line.
    pub buffered: Vec<u8>,
}

/// Shared map of pending connections waiting for `ConnectReady`.
///
/// Key is `conn_id`, value is a oneshot sender that delivers the data stream.
pub type PendingConnections = Arc<Mutex<HashMap<String, oneshot::Sender<DataStream>>>>;

/// Create a new empty pending connections map.
pub fn new_pending_connections() -> PendingConnections {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Maximum number of pending connections before pruning stale entries.
const MAX_PENDING: usize = 1024;

/// Register a pending connection and return a receiver that will deliver the
/// data stream when the container sends [`ConnectReady`].
///
/// The caller should generate a `conn_id` (UUID) and send a [`ConnectRequest`]
/// to the container, then await the returned receiver.
///
/// If there are already [`MAX_PENDING`] unresolved connections, stale entries
/// (whose receiver has been dropped due to timeout) are pruned first.
pub async fn register_pending(
    pending: &PendingConnections,
    conn_id: String,
) -> oneshot::Receiver<DataStream> {
    let (tx, rx) = oneshot::channel();
    let mut map = pending.lock().await;
    if map.len() >= MAX_PENDING {
        warn!(
            pending_count = map.len(),
            max = MAX_PENDING,
            "pending connections at capacity, pruning stale entries"
        );
        // Prune entries whose receiver has been dropped (timed out)
        map.retain(|_id, sender| !sender.is_closed());
    }
    map.insert(conn_id, tx);
    rx
}

/// Resolve a pending connection when a `ConnectReady` arrives on the data port.
///
/// Returns `true` if the `conn_id` was found and the stream was delivered,
/// `false` if no pending connection matched (stale or timed-out request).
pub async fn resolve_pending(
    pending: &PendingConnections,
    conn_id: &str,
    data: DataStream,
) -> bool {
    let sender = pending.lock().await.remove(conn_id);
    match sender {
        Some(tx) => {
            if tx.send(data).is_ok() {
                debug!(conn_id, "resolved pending connection");
                true
            } else {
                warn!(
                    conn_id,
                    "pending receiver dropped before data stream delivered"
                );
                false
            }
        }
        None => {
            warn!(conn_id, "no pending connection found (timed out or stale)");
            false
        }
    }
}

/// Cancel a pending connection (e.g. on `ConnectFailed`).
///
/// Removes the entry from the map so the awaiting side gets a
/// `RecvError` and can clean up.
pub async fn cancel_pending(pending: &PendingConnections, conn_id: &str) {
    if pending.lock().await.remove(conn_id).is_some() {
        debug!(conn_id, "cancelled pending connection");
    }
}

/// Wait for a data stream from a pending connection, then bridge the client
/// and data streams bidirectionally.
///
/// This function blocks until:
/// - The data stream arrives and both sides close (normal completion)
/// - The timeout expires (returns [`ProxyError::ConnectTimeout`])
/// - An I/O error occurs during copying
///
/// # Errors
///
/// Returns [`ProxyError`] on timeout or I/O failure.
pub async fn bridge_connection(
    conn_id: String,
    mut client_stream: TcpStream,
    data_rx: oneshot::Receiver<DataStream>,
    pending: &PendingConnections,
) -> Result<(u64, u64), ProxyError> {
    let data_stream = tokio::time::timeout(CONNECT_TIMEOUT, data_rx).await;

    match data_stream {
        Ok(Ok(DataStream {
            mut stream,
            buffered,
        })) => {
            // Write any data that was pre-read during the ConnectReady handshake.
            // The BufReader may have buffered data beyond the handshake line
            // (e.g., the first payload from the container's local service).
            let pre_read = buffered.len() as u64;
            if !buffered.is_empty() {
                debug!(conn_id, pre_read_bytes = pre_read, "flushing pre-read data to client");
                if let Err(source) = client_stream.write_all(&buffered).await {
                    return Err(ProxyError::Io { conn_id, source });
                }
            }

            debug!(conn_id, "bridging client and data streams");
            match copy_bidirectional(&mut client_stream, &mut stream).await {
                Ok(result) => {
                    info!(
                        conn_id,
                        client_to_container = result.0,
                        container_to_client = result.1 + pre_read,
                        "proxy connection completed"
                    );
                    Ok((result.0, result.1 + pre_read))
                }
                Err(source) => Err(ProxyError::Io { conn_id, source }),
            }
        }
        Ok(Err(_)) => {
            // Sender dropped — connection was cancelled or failed
            cancel_pending(pending, &conn_id).await;
            Err(ProxyError::ConnectFailed {
                conn_id,
                error: "pending connection cancelled".into(),
            })
        }
        Err(_) => {
            // Timeout
            cancel_pending(pending, &conn_id).await;
            Err(ProxyError::ConnectTimeout {
                conn_id,
                timeout_secs: CONNECT_TIMEOUT.as_secs(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Create a connected TCP stream pair via a temporary listener.
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (a, b) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (a.unwrap(), b.unwrap().0)
    }

    #[tokio::test]
    async fn register_and_resolve_pending() {
        let pending = new_pending_connections();
        let conn_id = "test-123".to_string();

        let rx = register_pending(&pending, conn_id.clone()).await;

        let (stream, _peer) = tcp_pair().await;
        let data = DataStream {
            stream,
            buffered: Vec::new(),
        };
        assert!(resolve_pending(&pending, &conn_id, data).await);

        let _data_stream = rx.await.unwrap();
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_unknown_conn_id_returns_false() {
        let pending = new_pending_connections();
        let (stream, _peer) = tcp_pair().await;
        let data = DataStream {
            stream,
            buffered: Vec::new(),
        };
        assert!(!resolve_pending(&pending, "unknown", data).await);
    }

    #[tokio::test]
    async fn cancel_pending_removes_entry() {
        let pending = new_pending_connections();
        let _rx = register_pending(&pending, "cancel-me".to_string()).await;

        assert_eq!(pending.lock().await.len(), 1);
        cancel_pending(&pending, "cancel-me").await;
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn bridge_connection_copies_data() {
        let pending = new_pending_connections();
        let conn_id = "bridge-test".to_string();

        let data_rx = register_pending(&pending, conn_id.clone()).await;

        // Set up a client pair (simulates client connecting to forwarded port)
        let (client_stream, mut client_local) = tcp_pair().await;

        // Set up a data pair (simulating reverse connection from container)
        let (data_stream, mut data_local) = tcp_pair().await;

        // Resolve the pending connection with the data stream
        let data = DataStream {
            stream: data_stream,
            buffered: Vec::new(),
        };
        assert!(resolve_pending(&pending, &conn_id, data).await);

        // Bridge in a background task
        let bridge_pending = pending.clone();
        let bridge_conn_id = conn_id.clone();
        let bridge_handle = tokio::spawn(async move {
            bridge_connection(bridge_conn_id, client_stream, data_rx, &bridge_pending).await
        });

        // Write from client side -> should appear on data side
        client_local.write_all(b"from client").await.unwrap();
        let mut buf = [0u8; 11];
        data_local.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from client");

        // Write from data side -> should appear on client side
        data_local.write_all(b"from container").await.unwrap();
        let mut buf = [0u8; 14];
        client_local.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from container");

        // Close both sides to let bridge complete
        drop(client_local);
        drop(data_local);

        let result = bridge_handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn bridge_connection_times_out() {
        let pending = new_pending_connections();
        let conn_id = "timeout-test".to_string();

        let data_rx = register_pending(&pending, conn_id.clone()).await;

        let (client_stream, _peer) = tcp_pair().await;

        // Don't resolve the pending -- it should time out
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            bridge_connection(conn_id, client_stream, data_rx, &pending),
        )
        .await;

        // Either our outer timeout or the inner 10s timeout will fire
        assert!(result.is_err() || result.unwrap().is_err());
    }
}
