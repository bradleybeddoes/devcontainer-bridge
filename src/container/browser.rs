//! Browser URL opening from inside a devcontainer.
//!
//! Provides [`open_url`], which connects to the host daemon's control channel,
//! sends an [`OpenUrl`](crate::protocol::Message::OpenUrl) message, and waits
//! for an [`OpenUrlAck`](crate::protocol::Message::OpenUrlAck) response.
//!
//! This powers the `dbr open <URL>` subcommand and the `dbr-open` hardlink
//! used as `BROWSER=dbr-open` inside devcontainers.

use std::net::SocketAddr;

use thiserror::Error;
use tracing::info;

use crate::config::Config;
use crate::container::resolve_host_addr;
use crate::control::{self, ControlError};
use crate::protocol::{self, Message};

/// Errors that can occur when opening a URL via the host daemon.
#[derive(Debug, Error)]
pub enum BrowserError {
    /// The URL failed validation (empty, bad scheme, too long, or control chars).
    #[error(transparent)]
    Validation(#[from] protocol::UrlValidationError),

    /// Failed to connect to the host daemon's control channel.
    #[error("could not connect to host daemon at {addr}: {source}. Is the host daemon running?")]
    Connect {
        /// The address we attempted to connect to.
        addr: SocketAddr,
        /// The underlying connection error.
        source: ControlError,
    },

    /// Failed to send the OpenUrl message.
    #[error("failed to send OpenUrl: {0}")]
    Send(ControlError),

    /// Failed to receive a response from the host daemon.
    #[error("failed to receive response: {0}")]
    Recv(ControlError),

    /// The host daemon reported that it could not open the URL.
    #[error("host daemon failed to open URL")]
    OpenFailed,

    /// Received an unexpected message type instead of OpenUrlAck.
    #[error("unexpected response from host daemon: {0:?}")]
    UnexpectedResponse(Message),

    /// Failed to resolve the host daemon address.
    #[error("could not resolve host address: {0}")]
    HostResolution(String),
}

/// Open a URL in the host browser by sending it to the host daemon.
///
/// Validates the URL (scheme and length), resolves the host daemon address
/// using the same chain as the container daemon (`host.docker.internal`,
/// gateway IP, etc.), sends [`Message::OpenUrl`], and waits for
/// [`Message::OpenUrlAck`].
///
/// # Arguments
///
/// * `url` — The URL to open (must be `http://` or `https://`, max 2048 chars).
/// * `control_port` — The host daemon's control channel port.
/// * `auth_token` — Authentication token for the control channel. Currently
///   reserved for future use (OpenUrl does not require registration).
///
/// # Errors
///
/// Returns [`BrowserError`] if the URL is invalid, the host daemon is
/// unreachable, or the host daemon fails to open the URL.
pub async fn open_url(url: &str, control_port: u16, auth_token: &str) -> Result<(), BrowserError> {
    // OpenUrl is sent without registration; token reserved for future use.
    let _ = auth_token;
    protocol::validate_open_url(url)?;

    let config = Config::from_env().map_err(|e| BrowserError::HostResolution(e.to_string()))?;
    let host = resolve_host_addr(&config)
        .await
        .map_err(|e| BrowserError::HostResolution(e.to_string()))?;
    let addr: SocketAddr = format!("{host}:{control_port}")
        .parse()
        .map_err(|e: std::net::AddrParseError| BrowserError::HostResolution(e.to_string()))?;
    let mut conn = control::connect(addr)
        .await
        .map_err(|e| BrowserError::Connect { addr, source: e })?;

    conn.send(&Message::OpenUrl {
        url: url.to_owned(),
    })
    .await
    .map_err(BrowserError::Send)?;

    let response = conn.recv().await.map_err(BrowserError::Recv)?;

    match response {
        Message::OpenUrlAck { success: true } => {
            info!(url, "URL opened successfully on host");
            Ok(())
        }
        Message::OpenUrlAck { success: false } => Err(BrowserError::OpenFailed),
        other => Err(BrowserError::UnexpectedResponse(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_url_validates_before_connecting() {
        // Should fail validation without ever trying to connect
        let result = open_url("ftp://bad", 19285, "").await;
        assert!(matches!(result, Err(BrowserError::Validation(_))));

        let result = open_url("", 19285, "").await;
        assert!(matches!(result, Err(BrowserError::Validation(_))));
    }

    #[tokio::test]
    async fn open_url_connection_refused() {
        // Use a high port unlikely to be in use. Depending on the environment,
        // this may fail with Connect (host resolved but port closed) or
        // HostResolution (host.docker.internal not available).
        let result = open_url("https://example.com", 19199, "").await;
        assert!(
            matches!(
                result,
                Err(BrowserError::Connect { .. }) | Err(BrowserError::HostResolution(_))
            ),
            "expected Connect or HostResolution error, got {result:?}"
        );
    }
}
