//! Token-based authentication for the devcontainer-bridge control channel.
//!
//! Provides generation, persistence, and resolution of authentication tokens.
//! Tokens are 64-character hex strings (32 random bytes) stored in
//! `~/.config/dbr/auth-token` with mode `0600`.
//!
//! # Token Resolution Order
//!
//! 1. `--auth-token` CLI flag (highest priority)
//! 2. `--auth-token-file` CLI flag / `DCBRIDGE_AUTH_TOKEN` env var
//! 3. `DCBRIDGE_AUTH_TOKEN_FILE` env var
//! 4. Default file path (`~/.config/dbr/auth-token` on host,
//!    `/run/secrets/dbr-auth-token` as container fallback)

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{debug, info};

/// Length of a valid auth token in hex characters (32 bytes = 64 hex chars).
const TOKEN_HEX_LENGTH: usize = 64;

/// Length of random bytes used to generate a token.
const TOKEN_BYTE_LENGTH: usize = 32;

/// File name for the auth token within the config directory.
const TOKEN_FILE_NAME: &str = "auth-token";

/// Environment variable for the auth token value.
pub const ENV_AUTH_TOKEN: &str = "DCBRIDGE_AUTH_TOKEN";

/// Environment variable for the auth token file path.
pub const ENV_AUTH_TOKEN_FILE: &str = "DCBRIDGE_AUTH_TOKEN_FILE";

/// Default container-side token file path (for bind-mount approach).
pub const DEFAULT_CONTAINER_TOKEN_PATH: &str = "/run/secrets/dbr-auth-token";

/// Errors that can occur during authentication operations.
#[derive(Debug, Error)]
pub enum AuthError {
    /// An I/O error occurred reading or writing the token file.
    #[error("auth token I/O error: {source}")]
    Io {
        /// The underlying I/O error.
        #[from]
        source: io::Error,
    },

    /// The token has an invalid format (not 64 hex characters).
    #[error(
        "invalid auth token format: expected {TOKEN_HEX_LENGTH} hex characters, got {length} chars"
    )]
    InvalidToken {
        /// The actual length of the token string.
        length: usize,
    },

    /// The token file was not found at the expected path.
    #[error("auth token file not found: {path}")]
    TokenNotFound {
        /// The path that was checked.
        path: PathBuf,
    },

    /// No token source was available (no flag, env var, or file).
    #[error(
        "no auth token available: set --auth-token, DCBRIDGE_AUTH_TOKEN, or ensure {path} exists"
    )]
    NoTokenSource {
        /// The default file path that was checked.
        path: PathBuf,
    },
}

/// Returns the default token file path (`~/.config/dbr/auth-token`).
///
/// # Errors
///
/// Returns [`AuthError::Io`] if the home directory cannot be determined.
pub fn token_file_path() -> Result<PathBuf, AuthError> {
    let config_dir = config_dir()?;
    Ok(config_dir.join(TOKEN_FILE_NAME))
}

/// Validates that a token string has the correct format (64 hex characters).
pub fn validate_token_format(token: &str) -> bool {
    token.len() == TOKEN_HEX_LENGTH && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Generates a new random authentication token (64 hex characters).
///
/// Uses `getrandom` for cryptographically secure random bytes.
///
/// # Errors
///
/// Returns [`AuthError::Io`] if the random number generator fails.
pub fn generate_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; TOKEN_BYTE_LENGTH];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Io {
        source: io::Error::other(e.to_string()),
    })?;
    let token = hex_encode(&bytes);
    debug!("generated new auth token");
    Ok(token)
}

/// Reads an auth token from a file, trimming whitespace.
///
/// # Errors
///
/// Returns [`AuthError::TokenNotFound`] if the file does not exist.
/// Returns [`AuthError::InvalidToken`] if the file contents are not a valid token.
/// Returns [`AuthError::Io`] on other I/O errors.
pub fn read_token_file(path: &Path) -> Result<String, AuthError> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let token = contents.trim().to_string();
            if !validate_token_format(&token) {
                return Err(AuthError::InvalidToken {
                    length: token.len(),
                });
            }
            debug!(path = %path.display(), "read auth token from file");
            Ok(token)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(AuthError::TokenNotFound {
            path: path.to_path_buf(),
        }),
        Err(e) => Err(AuthError::Io { source: e }),
    }
}

/// Writes an auth token to a file with restrictive permissions.
///
/// Creates parent directories (mode `0700`) if they don't exist.
/// Sets the token file to mode `0600` (owner read/write only).
///
/// # Errors
///
/// Returns [`AuthError::Io`] on filesystem errors.
/// Returns [`AuthError::InvalidToken`] if the token format is invalid.
pub fn write_token_file(path: &Path, token: &str) -> Result<(), AuthError> {
    if !validate_token_format(token) {
        return Err(AuthError::InvalidToken {
            length: token.len(),
        });
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_permissions(parent)?;
    }

    fs::write(path, format!("{token}\n"))?;
    set_file_permissions(path)?;

    info!(path = %path.display(), "wrote auth token to file");
    Ok(())
}

/// Reads the token from the file if it exists, otherwise generates a new one
/// and writes it to the file.
///
/// # Errors
///
/// Returns [`AuthError`] on I/O or generation errors.
pub fn ensure_token(path: &Path) -> Result<String, AuthError> {
    match read_token_file(path) {
        Ok(token) => Ok(token),
        Err(AuthError::TokenNotFound { .. }) => {
            let token = generate_token()?;
            write_token_file(path, &token)?;
            Ok(token)
        }
        Err(e) => Err(e),
    }
}

/// Resolves an auth token from multiple sources in priority order.
///
/// Resolution chain:
/// 1. `cli_token` — explicit token value from `--auth-token` flag
/// 2. `cli_token_file` — file path from `--auth-token-file` flag
/// 3. `DCBRIDGE_AUTH_TOKEN` environment variable
/// 4. `DCBRIDGE_AUTH_TOKEN_FILE` environment variable
/// 5. `default_file` — default token file path
///
/// # Errors
///
/// Returns [`AuthError::NoTokenSource`] if no source provides a valid token.
/// Returns [`AuthError::InvalidToken`] if a provided token has invalid format.
/// Returns [`AuthError::Io`] on file read errors.
pub fn resolve_token(
    cli_token: Option<&str>,
    cli_token_file: Option<&Path>,
    default_file: &Path,
) -> Result<String, AuthError> {
    // 1. --auth-token flag
    if let Some(token) = cli_token {
        let token = token.trim().to_string();
        if !validate_token_format(&token) {
            return Err(AuthError::InvalidToken {
                length: token.len(),
            });
        }
        debug!("using auth token from --auth-token flag");
        return Ok(token);
    }

    // 2. --auth-token-file flag
    if let Some(path) = cli_token_file {
        debug!(path = %path.display(), "trying auth token from --auth-token-file flag");
        return read_token_file(path);
    }

    // 3. DCBRIDGE_AUTH_TOKEN env var
    if let Ok(token) = std::env::var(ENV_AUTH_TOKEN) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            if !validate_token_format(&token) {
                return Err(AuthError::InvalidToken {
                    length: token.len(),
                });
            }
            debug!("using auth token from {ENV_AUTH_TOKEN} env var");
            return Ok(token);
        }
    }

    // 4. DCBRIDGE_AUTH_TOKEN_FILE env var
    if let Ok(path_str) = std::env::var(ENV_AUTH_TOKEN_FILE) {
        if !path_str.is_empty() {
            let path = PathBuf::from(&path_str);
            debug!(path = %path.display(), "trying auth token from {ENV_AUTH_TOKEN_FILE} env var");
            return read_token_file(&path);
        }
    }

    // 5. Default file path
    match read_token_file(default_file) {
        Ok(token) => Ok(token),
        Err(AuthError::TokenNotFound { .. }) => Err(AuthError::NoTokenSource {
            path: default_file.to_path_buf(),
        }),
        Err(e) => Err(e),
    }
}

/// Returns the config directory path (`~/.config/dbr/`).
fn config_dir() -> Result<PathBuf, AuthError> {
    let home = std::env::var("HOME").map_err(|_| AuthError::Io {
        source: io::Error::new(io::ErrorKind::NotFound, "HOME environment variable not set"),
    })?;
    Ok(PathBuf::from(home).join(".config").join("dbr"))
}

/// Hex-encodes a byte slice to a lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Sets file permissions to `0600` (owner read/write only).
#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

/// Sets directory permissions to `0700` (owner read/write/execute only).
#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o700);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_token_produces_valid_format() {
        let token = generate_token().unwrap();
        assert_eq!(token.len(), TOKEN_HEX_LENGTH);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_produces_unique_tokens() {
        let t1 = generate_token().unwrap();
        let t2 = generate_token().unwrap();
        assert_ne!(t1, t2);
    }

    #[test]
    fn validate_token_format_accepts_valid() {
        let token = "a".repeat(TOKEN_HEX_LENGTH);
        assert!(validate_token_format(&token));
        // Mixed case hex (exactly 64 chars)
        let token = "aAbBcCdDeEfF00112233445566778899aAbBcCdDeEfF00112233445566778899";
        assert_eq!(token.len(), TOKEN_HEX_LENGTH);
        assert!(validate_token_format(token));
    }

    #[test]
    fn validate_token_format_rejects_wrong_length() {
        assert!(!validate_token_format("abc123"));
        assert!(!validate_token_format(""));
        let too_long = "a".repeat(TOKEN_HEX_LENGTH + 1);
        assert!(!validate_token_format(&too_long));
    }

    #[test]
    fn validate_token_format_rejects_non_hex() {
        let token = "g".repeat(TOKEN_HEX_LENGTH);
        assert!(!validate_token_format(&token));
        // Valid length but contains non-hex
        let mut token = "a".repeat(TOKEN_HEX_LENGTH - 1);
        token.push('z');
        assert!(!validate_token_format(&token));
    }

    #[test]
    fn write_and_read_token_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let original = generate_token().unwrap();
        write_token_file(&path, &original).unwrap();

        let read_back = read_token_file(&path).unwrap();
        assert_eq!(original, read_back);
    }

    #[test]
    fn write_token_file_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dirs").join("auth-token");

        let token = generate_token().unwrap();
        write_token_file(&path, &token).unwrap();

        assert!(path.exists());
        let read_back = read_token_file(&path).unwrap();
        assert_eq!(token, read_back);
    }

    #[cfg(unix)]
    #[test]
    fn write_token_file_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let token = generate_token().unwrap();
        write_token_file(&path, &token).unwrap();

        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_token_file_sets_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("config");
        let path = parent.join("auth-token");

        let token = generate_token().unwrap();
        write_token_file(&path, &token).unwrap();

        let perms = fs::metadata(&parent).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o700);
    }

    #[test]
    fn read_token_file_missing_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent");

        let result = read_token_file(&path);
        assert!(matches!(result, Err(AuthError::TokenNotFound { .. })));
    }

    #[test]
    fn read_token_file_trims_whitespace() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let token = generate_token().unwrap();
        fs::write(&path, format!("  {token}  \n\n")).unwrap();

        let read_back = read_token_file(&path).unwrap();
        assert_eq!(token, read_back);
    }

    #[test]
    fn read_token_file_rejects_invalid_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        fs::write(&path, "not-a-valid-token\n").unwrap();

        let result = read_token_file(&path);
        assert!(matches!(result, Err(AuthError::InvalidToken { .. })));
    }

    #[test]
    fn write_token_file_rejects_invalid_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let result = write_token_file(&path, "too-short");
        assert!(matches!(result, Err(AuthError::InvalidToken { .. })));
    }

    #[test]
    fn ensure_token_generates_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let token = ensure_token(&path).unwrap();
        assert!(validate_token_format(&token));
        assert!(path.exists());
    }

    #[test]
    fn ensure_token_reads_when_present() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let original = generate_token().unwrap();
        write_token_file(&path, &original).unwrap();

        let token = ensure_token(&path).unwrap();
        assert_eq!(original, token);
    }

    #[test]
    fn ensure_token_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth-token");

        let t1 = ensure_token(&path).unwrap();
        let t2 = ensure_token(&path).unwrap();
        assert_eq!(t1, t2);
    }

    #[test]
    fn resolve_token_prefers_cli_flag() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("auth-token");
        let file_token = generate_token().unwrap();
        write_token_file(&file_path, &file_token).unwrap();

        let cli_token = generate_token().unwrap();
        let resolved = resolve_token(Some(&cli_token), None, &file_path).unwrap();
        assert_eq!(resolved, cli_token);
    }

    #[test]
    fn resolve_token_uses_cli_file() {
        let dir = TempDir::new().unwrap();

        let cli_file = dir.path().join("cli-token");
        let cli_token = generate_token().unwrap();
        write_token_file(&cli_file, &cli_token).unwrap();

        let default_file = dir.path().join("default-token");
        let default_token = generate_token().unwrap();
        write_token_file(&default_file, &default_token).unwrap();

        let resolved = resolve_token(None, Some(&cli_file), &default_file).unwrap();
        assert_eq!(resolved, cli_token);
    }

    #[test]
    fn resolve_token_falls_through_to_default_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("auth-token");
        let file_token = generate_token().unwrap();
        write_token_file(&file_path, &file_token).unwrap();

        let resolved = resolve_token(None, None, &file_path).unwrap();
        assert_eq!(resolved, file_token);
    }

    #[test]
    fn resolve_token_returns_no_source_when_nothing_available() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("nonexistent");

        let result = resolve_token(None, None, &file_path);
        assert!(matches!(result, Err(AuthError::NoTokenSource { .. })));
    }

    #[test]
    fn resolve_token_rejects_invalid_cli_token() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("auth-token");

        let result = resolve_token(Some("bad-token"), None, &file_path);
        assert!(matches!(result, Err(AuthError::InvalidToken { .. })));
    }

    #[test]
    fn hex_encode_works() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab, 0x12]), "00ffab12");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn token_file_path_returns_expected_suffix() {
        // Just verify the path ends with the expected components
        if let Ok(path) = token_file_path() {
            assert!(path.ends_with("auth-token"));
            assert!(path
                .parent()
                .unwrap()
                .ends_with(std::path::Path::new("dbr")));
        }
        // If HOME is not set, this may fail — that's fine for the test
    }
}
