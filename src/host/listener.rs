//! Per-port TCP listener management for the host daemon.
//!
//! Each forwarded port gets its own loopback listener. Listeners are started via
//! [`start_listener`] and shut down through a `tokio::sync::watch` channel.

use std::net::SocketAddr;

use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Errors that can occur when managing per-port listeners.
#[derive(Debug, Error)]
pub enum ListenerError {
    /// Failed to bind the TCP listener.
    #[error("failed to bind port {port}: {source}")]
    Bind {
        /// The port we attempted to bind.
        port: u16,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A client connection accepted by a port listener.
pub struct ClientConnection {
    /// The accepted TCP stream.
    pub stream: TcpStream,
    /// The container port being forwarded (the port clients connect to).
    pub container_port: u16,
    /// The remote address of the client.
    pub peer_addr: SocketAddr,
}

/// Bind a loopback TCP listener for the given port.
///
/// Tries `127.0.0.1:port` first. If that fails, falls back to `[::1]:port`.
/// **Never** binds to `0.0.0.0` or `[::]`.
///
/// IPv4 (`127.0.0.1`) is preferred because most clients — including browsers,
/// OAuth callbacks, and CLI tools like `nc` and `curl` — default to connecting
/// via IPv4 when given `localhost`.
///
/// # Errors
///
/// Returns [`ListenerError::Bind`] if all bind attempts fail.
async fn bind_loopback(port: u16) -> Result<TcpListener, ListenerError> {
    // Try 127.0.0.1 first (preferred — most clients default to IPv4)
    let ipv4_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    match TcpListener::bind(ipv4_addr).await {
        Ok(listener) => {
            debug!(port, addr = %ipv4_addr, "bound listener on 127.0.0.1");
            return Ok(listener);
        }
        Err(e) => {
            debug!(port, error = %e, "failed to bind 127.0.0.1, falling back to [::1]");
        }
    }

    // Fallback to [::1]
    let ipv6_addr: SocketAddr = ([0, 0, 0, 0, 0, 0, 0, 1], port).into();
    TcpListener::bind(ipv6_addr)
        .await
        .map_err(|source| ListenerError::Bind { port, source })
}

/// Start a TCP listener for a forwarded port on loopback.
///
/// Spawns a tokio task that accepts connections and sends them through the
/// `client_tx` channel. The listener can be shut down by sending `true` on
/// the `shutdown_rx` watch channel.
///
/// Returns the actual bound port and a join handle for the listener task.
///
/// # Errors
///
/// Returns [`ListenerError::Bind`] if the port cannot be bound.
pub async fn start_listener(
    port: u16,
    mut shutdown_rx: watch::Receiver<bool>,
    client_tx: mpsc::Sender<ClientConnection>,
) -> Result<(u16, tokio::task::JoinHandle<()>), ListenerError> {
    let listener = bind_loopback(port).await?;
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);

    info!(container_port = port, bound_port, "started port listener");

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                result = shutdown_rx.changed() => {
                    match result {
                        Ok(()) if *shutdown_rx.borrow() => {
                            info!(port, "port listener shutting down");
                            break;
                        }
                        Ok(()) => continue,
                        Err(_) => {
                            info!(port, "shutdown channel closed, stopping listener");
                            break;
                        }
                    }
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            debug!(port, %peer_addr, "accepted client connection");
                            let conn = ClientConnection {
                                stream,
                                container_port: port,
                                peer_addr,
                            };
                            if client_tx.send(conn).await.is_err() {
                                warn!(port, "client channel closed, stopping listener");
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(port, error = %e, "failed to accept connection");
                        }
                    }
                }
            }
        }
    });

    Ok((bound_port, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn bind_loopback_succeeds() {
        let listener = bind_loopback(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "listener must be bound to loopback, got {addr}"
        );
    }

    #[tokio::test]
    async fn start_listener_accepts_connections() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(16);

        let (bound_port, handle) = start_listener(0, shutdown_rx, tx).await.unwrap();
        assert!(bound_port > 0);

        // Connect a client — try 127.0.0.1 first since bind_loopback prefers IPv4
        let addrs: &[std::net::SocketAddr] = &[
            ([127, 0, 0, 1], bound_port).into(),
            ([0, 0, 0, 0, 0, 0, 0, 1], bound_port).into(),
        ];
        let mut client = tokio::net::TcpStream::connect(addrs).await.unwrap();
        client.write_all(b"hello").await.unwrap();

        // Should receive the connection on the channel
        let conn = rx.recv().await.expect("should receive client connection");
        assert_eq!(conn.container_port, 0); // port 0 was requested

        // Verify we can read from the accepted stream
        let mut buf = [0u8; 5];
        let mut stream = conn.stream;
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn start_listener_stops_on_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx, _rx) = mpsc::channel(16);

        let (_bound_port, handle) = start_listener(0, shutdown_rx, tx).await.unwrap();

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("listener should stop within timeout")
            .unwrap();
    }
}
