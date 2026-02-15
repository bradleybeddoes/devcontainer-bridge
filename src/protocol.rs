//! Protocol message types for the devcontainer-bridge control channel.
//!
//! All messages are serialized as JSON lines (newline-delimited JSON) and
//! tagged with a `"type"` field for discrimination.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during protocol message handling.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Failed to serialize a message to JSON.
    #[error("failed to serialize message: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Message exceeds the maximum allowed size.
    #[error("message exceeds maximum size of {max} bytes (got {actual})")]
    MessageTooLarge {
        /// Maximum allowed size in bytes.
        max: usize,
        /// Actual size in bytes.
        actual: usize,
    },
}

/// Maximum allowed URL length in characters for [`Message::OpenUrl`].
pub const MAX_URL_LENGTH: usize = 2048;

/// Errors from URL validation for [`Message::OpenUrl`].
#[derive(Debug, Error)]
pub enum UrlValidationError {
    /// The URL is empty.
    #[error("URL is empty")]
    Empty,

    /// The URL scheme is not `http://` or `https://`.
    #[error("invalid URL scheme: only http:// and https:// are allowed")]
    InvalidScheme,

    /// The URL exceeds [`MAX_URL_LENGTH`] characters.
    #[error("URL too long: {len} chars exceeds {max} char limit")]
    TooLong {
        /// Actual length of the URL.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },

    /// The URL contains ASCII control characters.
    #[error("URL contains invalid characters (control characters are not allowed)")]
    InvalidCharacters,
}

/// Validate that a URL is safe for the [`Message::OpenUrl`] protocol.
///
/// Checks that the URL is non-empty, uses `http://` or `https://`,
/// does not exceed [`MAX_URL_LENGTH`], and contains no ASCII control
/// characters.
///
/// # Errors
///
/// Returns [`UrlValidationError`] describing the first violated constraint.
pub fn validate_open_url(url: &str) -> Result<(), UrlValidationError> {
    if url.is_empty() {
        return Err(UrlValidationError::Empty);
    }

    if url.len() > MAX_URL_LENGTH {
        return Err(UrlValidationError::TooLong {
            len: url.len(),
            max: MAX_URL_LENGTH,
        });
    }

    if url.chars().any(|c| c.is_ascii_control()) {
        return Err(UrlValidationError::InvalidCharacters);
    }

    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(UrlValidationError::InvalidScheme);
    }

    Ok(())
}

/// Transport protocol for a forwarded port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// TCP protocol (the only supported protocol in v1).
    Tcp,
}

/// Information about a single active port forward, used in [`Message::ListResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardInfo {
    /// The container that owns this forward.
    pub container_id: String,
    /// Human-readable hostname of the container.
    pub hostname: String,
    /// The port being forwarded inside the container.
    pub port: u16,
    /// The port bound on the host.
    pub host_port: u16,
    /// The transport protocol.
    pub protocol: Protocol,
    /// Name of the process listening on the port, if known.
    pub process_name: Option<String>,
    /// PID of the process listening on the port, if known.
    pub pid: Option<u32>,
    /// ISO 8601 timestamp of when the forward was established.
    pub since: String,
}

/// A control channel message exchanged between host and container daemons.
///
/// Messages are serialized as JSON with an internally-tagged `"type"` field.
///
/// # Examples
///
/// ```
/// use dbr::protocol::Message;
///
/// let msg = Message::Ping;
/// let json = serde_json::to_string(&msg).unwrap();
/// assert_eq!(json, r#"{"type":"Ping"}"#);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Message {
    /// Container registers itself with the host daemon.
    Register {
        /// Unique identifier for the container.
        container_id: String,
        /// Human-readable hostname.
        hostname: String,
    },

    /// Host acknowledges a container registration.
    RegisterAck {
        /// Whether the registration succeeded.
        success: bool,
    },

    /// Container requests a port to be forwarded to the host.
    Forward {
        /// The port to forward.
        port: u16,
        /// The transport protocol.
        protocol: Protocol,
        /// Name of the process listening on the port, if known.
        process_name: Option<String>,
        /// PID of the listening process, if known.
        pid: Option<u32>,
    },

    /// Host acknowledges a forward request.
    ForwardAck {
        /// The container port that was requested.
        port: u16,
        /// Whether the forward succeeded.
        success: bool,
        /// The actual port bound on the host (may differ if there was a conflict).
        host_port: u16,
    },

    /// Container requests removal of a port forward.
    Unforward {
        /// The port to stop forwarding.
        port: u16,
    },

    /// Host asks the container to open a reverse data connection for a client.
    ConnectRequest {
        /// The container port the client wants to reach.
        port: u16,
        /// Unique identifier for this connection (UUID format).
        conn_id: String,
    },

    /// Container signals that a reverse data connection is ready.
    ///
    /// Sent on the **data channel** (port 19286), not the control channel.
    ConnectReady {
        /// The connection identifier matching the original [`Message::ConnectRequest`].
        conn_id: String,
    },

    /// Container reports that it could not fulfil a connect request.
    ConnectFailed {
        /// The connection identifier matching the original [`Message::ConnectRequest`].
        conn_id: String,
        /// Human-readable error description.
        error: String,
    },

    /// Container asks the host to open a URL in the host browser.
    OpenUrl {
        /// The URL to open (must be http:// or https://).
        url: String,
    },

    /// Host acknowledges a URL open request.
    OpenUrlAck {
        /// Whether the URL was successfully opened.
        success: bool,
    },

    /// Keepalive ping (either direction).
    Ping,

    /// Keepalive pong response (either direction).
    Pong,

    /// CLI requests a list of all active forwards.
    ListRequest,

    /// Host responds with all active forwards.
    ListResponse {
        /// All currently active port forwards across all containers.
        forwards: Vec<ForwardInfo>,
    },
}

/// Serialize a message to a JSON string (without trailing newline).
///
/// # Errors
///
/// Returns [`ProtocolError::Serialization`] if the message cannot be serialized.
pub fn serialize_message(msg: &Message) -> Result<String, ProtocolError> {
    serde_json::to_string(msg).map_err(ProtocolError::Serialization)
}

/// Deserialize a message from a JSON string.
///
/// # Errors
///
/// Returns [`ProtocolError::Serialization`] if the string is not valid JSON
/// or does not match any known message type.
pub fn deserialize_message(s: &str) -> Result<Message, ProtocolError> {
    serde_json::from_str(s).map_err(ProtocolError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_register() {
        let msg = Message::Register {
            container_id: "abc123".into(),
            hostname: "dev".into(),
        };
        let json = serialize_message(&msg).unwrap();
        assert!(json.contains(r#""type":"Register""#));
        assert!(json.contains(r#""container_id":"abc123""#));
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_register_ack() {
        let msg = Message::RegisterAck { success: true };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_forward() {
        let msg = Message::Forward {
            port: 8080,
            protocol: Protocol::Tcp,
            process_name: Some("node".into()),
            pid: Some(1234),
        };
        let json = serialize_message(&msg).unwrap();
        assert!(json.contains(r#""type":"Forward""#));
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_forward_no_optional_fields() {
        let msg = Message::Forward {
            port: 3000,
            protocol: Protocol::Tcp,
            process_name: None,
            pid: None,
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_forward_ack() {
        let msg = Message::ForwardAck {
            port: 8080,
            success: true,
            host_port: 8081,
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_unforward() {
        let msg = Message::Unforward { port: 8080 };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_connect_request() {
        let msg = Message::ConnectRequest {
            port: 8080,
            conn_id: "uuid-1234".into(),
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_connect_ready() {
        let msg = Message::ConnectReady {
            conn_id: "uuid-1234".into(),
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_connect_failed() {
        let msg = Message::ConnectFailed {
            conn_id: "uuid-1234".into(),
            error: "connection refused".into(),
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_open_url() {
        let msg = Message::OpenUrl {
            url: "http://localhost:8080/auth/callback".into(),
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_open_url_ack() {
        let msg = Message::OpenUrlAck { success: true };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_ping_pong() {
        for msg in [Message::Ping, Message::Pong] {
            let json = serialize_message(&msg).unwrap();
            let decoded = deserialize_message(&json).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn roundtrip_list_request() {
        let msg = Message::ListRequest;
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_list_response() {
        let msg = Message::ListResponse {
            forwards: vec![ForwardInfo {
                container_id: "abc123".into(),
                hostname: "dev".into(),
                port: 8080,
                host_port: 8080,
                protocol: Protocol::Tcp,
                process_name: Some("node".into()),
                pid: Some(1234),
                since: "2026-01-01T00:00:00Z".into(),
            }],
        };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn list_response_empty_forwards() {
        let msg = Message::ListResponse { forwards: vec![] };
        let json = serialize_message(&msg).unwrap();
        let decoded = deserialize_message(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn tagged_format_is_correct() {
        let json = serialize_message(&Message::Ping).unwrap();
        assert_eq!(json, r#"{"type":"Ping"}"#);

        let json = serialize_message(&Message::Pong).unwrap();
        assert_eq!(json, r#"{"type":"Pong"}"#);
    }

    #[test]
    fn unknown_type_returns_error() {
        let result = deserialize_message(r#"{"type":"Unknown"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_json_returns_error() {
        let result = deserialize_message("not json");
        assert!(result.is_err());
    }

    #[test]
    fn missing_type_field_returns_error() {
        let result = deserialize_message(r#"{"port": 8080}"#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        // With deny_unknown_fields, extra fields in a known message type
        // should cause a deserialization error.
        let result = deserialize_message(
            r#"{"type":"Forward","port":8080,"protocol":"Tcp","process_name":null,"pid":null,"extra_field":"bad"}"#,
        );
        assert!(result.is_err(), "unknown fields should be rejected");
    }

    #[test]
    fn negative_port_rejected() {
        // u16 cannot be negative — serde should reject this
        let result = deserialize_message(
            r#"{"type":"Forward","port":-1,"protocol":"Tcp","process_name":null,"pid":null}"#,
        );
        assert!(result.is_err(), "negative port should be rejected");
    }

    #[test]
    fn oversized_port_rejected() {
        // Port > 65535 should not fit in u16
        let result = deserialize_message(
            r#"{"type":"Forward","port":70000,"protocol":"Tcp","process_name":null,"pid":null}"#,
        );
        assert!(result.is_err(), "port > 65535 should be rejected");
    }

    // --- validate_open_url tests ---

    #[test]
    fn validate_http_url() {
        assert!(validate_open_url("http://localhost:8080/path").is_ok());
    }

    #[test]
    fn validate_https_url() {
        assert!(validate_open_url("https://example.com/auth/callback").is_ok());
    }

    #[test]
    fn validate_case_insensitive_scheme() {
        assert!(validate_open_url("HTTP://localhost:8080").is_ok());
        assert!(validate_open_url("Https://example.com").is_ok());
        assert!(validate_open_url("hTtP://example.com/path").is_ok());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(
            validate_open_url(""),
            Err(UrlValidationError::Empty)
        ));
    }

    #[test]
    fn validate_rejects_ftp() {
        assert!(matches!(
            validate_open_url("ftp://example.com/file"),
            Err(UrlValidationError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_file() {
        assert!(matches!(
            validate_open_url("file:///etc/passwd"),
            Err(UrlValidationError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_javascript() {
        assert!(matches!(
            validate_open_url("javascript:alert(1)"),
            Err(UrlValidationError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_control_characters() {
        assert!(matches!(
            validate_open_url("http://example.com/path\nHeader: injected"),
            Err(UrlValidationError::InvalidCharacters)
        ));
        assert!(matches!(
            validate_open_url("http://example.com/\0null"),
            Err(UrlValidationError::InvalidCharacters)
        ));
    }

    #[test]
    fn validate_rejects_too_long() {
        let long_url = format!("http://example.com/{}", "a".repeat(MAX_URL_LENGTH));
        assert!(matches!(
            validate_open_url(&long_url),
            Err(UrlValidationError::TooLong { .. })
        ));
    }

    #[test]
    fn validate_accepts_max_length() {
        let url = format!("http://x.co/{}", "a".repeat(MAX_URL_LENGTH - 12));
        assert_eq!(url.len(), MAX_URL_LENGTH);
        assert!(validate_open_url(&url).is_ok());
    }
}
