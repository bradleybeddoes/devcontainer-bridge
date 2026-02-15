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
use crate::protocol::Message;

/// Maximum allowed URL length in characters.
const MAX_URL_LENGTH: usize = 2048;

/// Errors that can occur when opening a URL via the host daemon.
#[derive(Debug, Error)]
pub enum BrowserError {
    /// The URL is empty.
    #[error("URL is empty")]
    EmptyUrl,

    /// The URL does not start with `http://` or `https://`.
    #[error("invalid URL scheme: only http:// and https:// are allowed")]
    InvalidScheme,

    /// The URL exceeds the maximum allowed length.
    #[error("URL exceeds maximum length of {max} characters (got {actual})")]
    UrlTooLong {
        /// Maximum allowed length.
        max: usize,
        /// Actual length of the provided URL.
        actual: usize,
    },

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
///
/// # Errors
///
/// Returns [`BrowserError`] if the URL is invalid, the host daemon is
/// unreachable, or the host daemon fails to open the URL.
pub async fn open_url(url: &str, control_port: u16) -> Result<(), BrowserError> {
    validate_url(url)?;

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

/// Validate that a URL meets the scheme and length requirements.
///
/// # Errors
///
/// Returns [`BrowserError::EmptyUrl`] if the URL is empty,
/// [`BrowserError::InvalidScheme`] if it does not start with `http://` or
/// `https://`, or [`BrowserError::UrlTooLong`] if it exceeds
/// [`MAX_URL_LENGTH`].
fn validate_url(url: &str) -> Result<(), BrowserError> {
    if url.is_empty() {
        return Err(BrowserError::EmptyUrl);
    }

    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(BrowserError::InvalidScheme);
    }

    if url.len() > MAX_URL_LENGTH {
        return Err(BrowserError::UrlTooLong {
            max: MAX_URL_LENGTH,
            actual: url.len(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_http_url() {
        assert!(validate_url("http://localhost:8080/callback").is_ok());
    }

    #[test]
    fn valid_https_url() {
        assert!(validate_url("https://example.com/auth").is_ok());
    }

    #[test]
    fn empty_url_rejected() {
        let err = validate_url("").unwrap_err();
        assert!(matches!(err, BrowserError::EmptyUrl));
    }

    #[test]
    fn ftp_scheme_rejected() {
        let err = validate_url("ftp://example.com").unwrap_err();
        assert!(matches!(err, BrowserError::InvalidScheme));
    }

    #[test]
    fn file_scheme_rejected() {
        let err = validate_url("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, BrowserError::InvalidScheme));
    }

    #[test]
    fn no_scheme_rejected() {
        let err = validate_url("example.com").unwrap_err();
        assert!(matches!(err, BrowserError::InvalidScheme));
    }

    #[test]
    fn case_insensitive_scheme_accepted() {
        assert!(validate_url("HTTP://example.com").is_ok());
        assert!(validate_url("Https://example.com").is_ok());
        assert!(validate_url("hTtP://example.com/path").is_ok());
    }

    #[test]
    fn data_scheme_rejected() {
        let err = validate_url("data:text/html,<h1>hi</h1>").unwrap_err();
        assert!(matches!(err, BrowserError::InvalidScheme));
    }

    #[test]
    fn javascript_scheme_rejected() {
        let err = validate_url("javascript:alert(1)").unwrap_err();
        assert!(matches!(err, BrowserError::InvalidScheme));
    }

    #[test]
    fn url_at_max_length_accepted() {
        let url = format!("https://example.com/{}", "a".repeat(MAX_URL_LENGTH - 20));
        // Just under or at limit — should pass as long as total <= MAX_URL_LENGTH
        if url.len() <= MAX_URL_LENGTH {
            assert!(validate_url(&url).is_ok());
        }
    }

    #[test]
    fn url_over_max_length_rejected() {
        let url = format!("https://x.co/{}", "a".repeat(MAX_URL_LENGTH));
        let err = validate_url(&url).unwrap_err();
        match err {
            BrowserError::UrlTooLong { max, actual } => {
                assert_eq!(max, MAX_URL_LENGTH);
                assert_eq!(actual, url.len());
            }
            other => panic!("expected UrlTooLong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_url_validates_before_connecting() {
        // Should fail validation without ever trying to connect
        let result = open_url("ftp://bad", 19285).await;
        assert!(matches!(result, Err(BrowserError::InvalidScheme)));

        let result = open_url("", 19285).await;
        assert!(matches!(result, Err(BrowserError::EmptyUrl)));
    }

    #[tokio::test]
    async fn open_url_connection_refused() {
        // Use a high port unlikely to be in use. Depending on the environment,
        // this may fail with Connect (host resolved but port closed) or
        // HostResolution (host.docker.internal not available).
        let result = open_url("https://example.com", 19199).await;
        assert!(
            matches!(
                result,
                Err(BrowserError::Connect { .. }) | Err(BrowserError::HostResolution(_))
            ),
            "expected Connect or HostResolution error, got {result:?}"
        );
    }
}
