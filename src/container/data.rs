//! Reverse data connection handler for the container daemon.
//!
//! When the host daemon sends a [`ConnectRequest`], this module:
//! 1. Connects to the local port inside the container
//! 2. Opens a new TCP connection to the host's data port (19286)
//! 3. Sends [`ConnectReady`] on the data connection
//! 4. Bridges the local and data connections bidirectionally
//!
//! [`ConnectRequest`]: crate::protocol::Message::ConnectRequest
//! [`ConnectReady`]: crate::protocol::Message::ConnectReady

use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::control::ControlError;
use crate::protocol::Message;

/// Errors that can occur when handling a data connection.
#[derive(Debug, Error)]
pub enum DataError {
    /// Failed to connect to the local port inside the container.
    #[error("failed to connect to local port {port}: {source}")]
    LocalConnect {
        /// The port we tried to connect to.
        port: u16,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to open the reverse data connection to the host.
    #[error("failed to connect to host data port {addr}: {source}")]
    HostConnect {
        /// The host data address we tried to connect to.
        addr: SocketAddr,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to send the handshake message on the data connection.
    #[error("failed to send ConnectReady handshake: {0}")]
    Handshake(std::io::Error),

    /// Failed to bridge the two connections.
    #[error("bridge I/O error: {0}")]
    Bridge(std::io::Error),

    /// Failed to send ConnectFailed on the control channel.
    #[error("failed to send ConnectFailed: {0}")]
    ControlSend(#[from] ControlError),
}

/// Handle a single [`ConnectRequest`] by bridging a local port to a reverse
/// data connection back to the host.
///
/// This function is designed to be spawned as a tokio task for each incoming
/// connection request.
///
/// # Arguments
///
/// * `port` — The container-local port to connect to.
/// * `conn_id` — The unique connection identifier from the host.
/// * `host_data_addr` — The host's data channel address to open the reverse connection to.
///
/// # Returns
///
/// Returns `Ok(())` when the bidirectional copy completes (either side closed),
/// or `Err(DataError)` describing what went wrong. Callers should log or
/// report the error via `ConnectFailed` on the control channel.
///
/// [`ConnectRequest`]: crate::protocol::Message::ConnectRequest
pub async fn handle_connect_request(
    port: u16,
    conn_id: String,
    host_data_addr: SocketAddr,
) -> Result<(), DataError> {
    // Step 1: Connect to the local port inside the container
    let local_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    debug!(port, %conn_id, "connecting to local port");

    let mut local_stream = TcpStream::connect(local_addr)
        .await
        .map_err(|e| DataError::LocalConnect { port, source: e })?;

    // Step 2: Open reverse data connection to the host
    debug!(%host_data_addr, %conn_id, "opening reverse data connection to host");

    let mut data_stream =
        TcpStream::connect(host_data_addr)
            .await
            .map_err(|e| DataError::HostConnect {
                addr: host_data_addr,
                source: e,
            })?;

    // Step 3: Send ConnectReady handshake as a JSON line on the data connection
    let ready_msg = Message::ConnectReady {
        conn_id: conn_id.clone(),
    };
    let mut json = serde_json::to_string(&ready_msg).map_err(|e| {
        DataError::Handshake(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;
    json.push('\n');

    data_stream
        .write_all(json.as_bytes())
        .await
        .map_err(DataError::Handshake)?;
    data_stream.flush().await.map_err(DataError::Handshake)?;

    info!(%conn_id, port, "data connection ready, starting bridge");

    // Step 4: Bridge bidirectionally
    copy_bidirectional(&mut local_stream, &mut data_stream)
        .await
        .inspect(|(to_local, to_data)| {
            debug!(%conn_id, port, bytes_to_local = to_local, bytes_to_remote = to_data, "bridge completed");
        })
        .inspect_err(|e| {
            warn!(%conn_id, port, error = %e, "bridge error");
        })
        .map(|_| ())
        .map_err(DataError::Bridge)
}

/// Spawn a tokio task to handle a [`Message::ConnectRequest`].
///
/// On failure, the returned error string can be sent as a `ConnectFailed`
/// message on the control channel.
///
/// # Arguments
///
/// * `port` — The container-local port to connect to.
/// * `conn_id` — The unique connection identifier.
/// * `host_data_addr` — The host data channel address.
pub fn spawn_connect_handler(
    port: u16,
    conn_id: String,
    host_data_addr: SocketAddr,
) -> tokio::task::JoinHandle<Result<(), DataError>> {
    tokio::spawn(async move {
        let result = handle_connect_request(port, conn_id.clone(), host_data_addr).await;
        if let Err(ref e) = result {
            error!(%conn_id, port, error = %e, "connect request failed");
        }
        result
    })
}
