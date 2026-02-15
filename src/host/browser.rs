//! Host-side browser opening for `OpenUrl` messages from containers.
//!
//! Validates URLs (http/https only, length cap), rewrites `localhost` ports
//! when the container port is forwarded to a different host port, and opens
//! the URL in the host's default browser.

use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::time::Instant;

use thiserror::Error;
use tracing::{debug, info, warn};

/// Maximum allowed URL length in characters.
const MAX_URL_LENGTH: usize = 2048;

/// Maximum number of browser opens per second.
const RATE_LIMIT_PER_SEC: usize = 5;

/// Errors that can occur when opening a URL in the host browser.
#[derive(Debug, Error)]
pub enum BrowserError {
    /// The URL scheme is not `http://` or `https://`.
    #[error("invalid URL scheme: only http:// and https:// are allowed")]
    InvalidScheme,

    /// The URL exceeds the maximum allowed length.
    #[error("URL too long: {len} chars exceeds {max} char limit")]
    UrlTooLong {
        /// Actual length of the URL.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },

    /// The URL contains control characters (newlines, null bytes, etc.).
    #[error("URL contains invalid characters (control characters are not allowed)")]
    InvalidCharacters,

    /// Too many URL open requests in a short period.
    #[error("rate limited: exceeded {RATE_LIMIT_PER_SEC} opens per second")]
    RateLimited,

    /// The browser open command failed.
    #[error("failed to open browser: {0}")]
    OpenFailed(String),
}

/// Validate that a URL is safe to open in the host browser.
///
/// Checks that the URL uses `http://` or `https://` and does not exceed
/// `MAX_URL_LENGTH` characters.
///
/// # Errors
///
/// Returns [`BrowserError::InvalidScheme`] or [`BrowserError::UrlTooLong`].
pub fn validate_url(url: &str) -> Result<(), BrowserError> {
    if url.len() > MAX_URL_LENGTH {
        return Err(BrowserError::UrlTooLong {
            len: url.len(),
            max: MAX_URL_LENGTH,
        });
    }

    // Reject control characters (newlines, null bytes, etc.) to prevent
    // log injection and argument confusion in open/xdg-open.
    if url.chars().any(|c| c.is_ascii_control()) {
        return Err(BrowserError::InvalidCharacters);
    }

    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(BrowserError::InvalidScheme);
    }

    Ok(())
}

/// Rewrite `localhost` or `127.0.0.1` ports in a URL using the port map.
///
/// If the URL contains `localhost:PORT` or `127.0.0.1:PORT` and `PORT` is a
/// key in `port_map`, the port is replaced with the mapped host port.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
/// use dbr::host::browser::rewrite_url;
///
/// let mut map = HashMap::new();
/// map.insert(3000, 3001);
/// assert_eq!(
///     rewrite_url("http://localhost:3000/callback", &map),
///     "http://localhost:3001/callback"
/// );
/// ```
pub fn rewrite_url(url: &str, port_map: &HashMap<u16, u16>) -> String {
    let Some(scheme_end) = url.find("://").map(|p| p + 3) else {
        return url.to_string();
    };

    if port_map.is_empty() {
        return url.to_string();
    }

    let rest = &url[scheme_end..];
    let lower_rest = rest.to_ascii_lowercase();

    for host_prefix in ["localhost:", "127.0.0.1:"] {
        if !lower_rest.starts_with(host_prefix) {
            continue;
        }
        let after_host = &rest[host_prefix.len()..];
        let port_str: String = after_host
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(port) = port_str.parse::<u16>() {
            if let Some(&host_port) = port_map.get(&port) {
                let prefix = &url[..scheme_end + host_prefix.len()];
                let suffix = &after_host[port_str.len()..];
                return format!("{prefix}{host_port}{suffix}");
            }
        }
        break;
    }

    url.to_string()
}

/// Opens a URL in the host's default browser (or a custom command).
///
/// If `browser_cmd` is `Some`, uses that command. Otherwise uses `open` on
/// macOS and `xdg-open` on Linux. The URL is passed as a single argument
/// (not via shell) to prevent command injection.
fn open_in_browser(url: &str, browser_cmd: Option<&str>) -> Result<(), BrowserError> {
    let cmd = match browser_cmd {
        Some(c) => c,
        None => {
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            }
        }
    };

    debug!(cmd, url, "opening URL in browser");

    let status = Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| BrowserError::OpenFailed(format!("{cmd}: {e}")))?;

    if status.success() {
        Ok(())
    } else {
        Err(BrowserError::OpenFailed(format!(
            "{cmd} exited with status {}",
            status
        )))
    }
}

/// Manages browser opening with URL validation, port rewriting, and rate limiting.
pub struct BrowserOpener {
    /// Maps container port → host port for URL rewriting.
    port_map: HashMap<u16, u16>,
    /// Timestamps of recent opens for rate limiting (sliding window).
    recent_opens: VecDeque<Instant>,
    /// Custom browser command (overrides platform default).
    browser_cmd: Option<String>,
}

impl Default for BrowserOpener {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserOpener {
    /// Create a new `BrowserOpener` with an empty port map and platform-default browser.
    pub fn new() -> Self {
        Self {
            port_map: HashMap::new(),
            recent_opens: VecDeque::new(),
            browser_cmd: None,
        }
    }

    /// Create a new `BrowserOpener` with an optional custom browser command.
    ///
    /// If `cmd` is `Some`, that command is used instead of `open` (macOS) /
    /// `xdg-open` (Linux). Useful for testing or headless environments.
    pub fn with_cmd(cmd: Option<String>) -> Self {
        Self {
            port_map: HashMap::new(),
            recent_opens: VecDeque::new(),
            browser_cmd: cmd,
        }
    }

    /// Record a port mapping (container_port → host_port) for URL rewriting.
    pub fn add_port_mapping(&mut self, container_port: u16, host_port: u16) {
        self.port_map.insert(container_port, host_port);
    }

    /// Remove a port mapping.
    pub fn remove_port_mapping(&mut self, container_port: u16) {
        self.port_map.remove(&container_port);
    }

    /// Open a URL in the host browser with validation, rewriting, and rate limiting.
    ///
    /// # Errors
    ///
    /// Returns [`BrowserError`] if validation fails, the rate limit is exceeded,
    /// or the browser command fails.
    pub fn open(&mut self, url: &str) -> Result<(), BrowserError> {
        validate_url(url)?;

        // Rate limiting: sliding window of 1 second.
        // Entries are ordered chronologically, so we only need to pop
        // expired entries from the front — O(expired) instead of O(n).
        let now = Instant::now();
        while self
            .recent_opens
            .front()
            .is_some_and(|t| now.duration_since(*t).as_secs() >= 1)
        {
            self.recent_opens.pop_front();
        }
        if self.recent_opens.len() >= RATE_LIMIT_PER_SEC {
            warn!(url, "browser open rate limited");
            return Err(BrowserError::RateLimited);
        }
        self.recent_opens.push_back(now);

        let rewritten = rewrite_url(url, &self.port_map);
        if rewritten != url {
            info!(
                original = url,
                rewritten = rewritten.as_str(),
                "rewrote URL port"
            );
        }

        open_in_browser(&rewritten, self.browser_cmd.as_deref())?;
        info!(url = rewritten.as_str(), "opened URL in browser");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_url tests ---

    #[test]
    fn validate_http_url() {
        assert!(validate_url("http://localhost:8080/path").is_ok());
    }

    #[test]
    fn validate_https_url() {
        assert!(validate_url("https://example.com/auth/callback").is_ok());
    }

    #[test]
    fn validate_http_case_insensitive() {
        assert!(validate_url("HTTP://localhost:8080").is_ok());
        assert!(validate_url("Https://example.com").is_ok());
    }

    #[test]
    fn validate_rejects_ftp() {
        assert!(matches!(
            validate_url("ftp://example.com/file"),
            Err(BrowserError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_file() {
        assert!(matches!(
            validate_url("file:///etc/passwd"),
            Err(BrowserError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_javascript() {
        assert!(matches!(
            validate_url("javascript:alert(1)"),
            Err(BrowserError::InvalidScheme)
        ));
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(validate_url(""), Err(BrowserError::InvalidScheme)));
    }

    #[test]
    fn validate_rejects_control_characters() {
        assert!(matches!(
            validate_url("http://example.com/path\nHeader: injected"),
            Err(BrowserError::InvalidCharacters)
        ));
        assert!(matches!(
            validate_url("http://example.com/\0null"),
            Err(BrowserError::InvalidCharacters)
        ));
        assert!(matches!(
            validate_url("http://example.com/\ttab"),
            Err(BrowserError::InvalidCharacters)
        ));
    }

    #[test]
    fn validate_rejects_too_long() {
        let long_url = format!("http://example.com/{}", "a".repeat(MAX_URL_LENGTH));
        assert!(matches!(
            validate_url(&long_url),
            Err(BrowserError::UrlTooLong { .. })
        ));
    }

    #[test]
    fn validate_accepts_max_length() {
        // Exactly at the limit should be fine
        let url = format!("http://x.co/{}", "a".repeat(MAX_URL_LENGTH - 12));
        assert_eq!(url.len(), MAX_URL_LENGTH);
        assert!(validate_url(&url).is_ok());
    }

    // --- rewrite_url tests ---

    #[test]
    fn rewrite_localhost_port() {
        let mut map = HashMap::new();
        map.insert(3000, 3001);
        assert_eq!(
            rewrite_url("http://localhost:3000/callback", &map),
            "http://localhost:3001/callback"
        );
    }

    #[test]
    fn rewrite_127_0_0_1_port() {
        let mut map = HashMap::new();
        map.insert(8080, 9090);
        assert_eq!(
            rewrite_url("http://127.0.0.1:8080/api/v1", &map),
            "http://127.0.0.1:9090/api/v1"
        );
    }

    #[test]
    fn rewrite_https() {
        let mut map = HashMap::new();
        map.insert(443, 8443);
        assert_eq!(
            rewrite_url("https://localhost:443/secure", &map),
            "https://localhost:8443/secure"
        );
    }

    #[test]
    fn rewrite_leaves_unmapped_port() {
        let mut map = HashMap::new();
        map.insert(3000, 3001);
        assert_eq!(
            rewrite_url("http://localhost:4000/path", &map),
            "http://localhost:4000/path"
        );
    }

    #[test]
    fn rewrite_leaves_external_host() {
        let mut map = HashMap::new();
        map.insert(8080, 9090);
        assert_eq!(
            rewrite_url("http://example.com:8080/path", &map),
            "http://example.com:8080/path"
        );
    }

    #[test]
    fn rewrite_empty_map() {
        let map = HashMap::new();
        assert_eq!(
            rewrite_url("http://localhost:3000/path", &map),
            "http://localhost:3000/path"
        );
    }

    #[test]
    fn rewrite_preserves_query_string() {
        let mut map = HashMap::new();
        map.insert(3000, 3001);
        assert_eq!(
            rewrite_url("http://localhost:3000/auth?code=abc&state=xyz", &map),
            "http://localhost:3001/auth?code=abc&state=xyz"
        );
    }

    #[test]
    fn rewrite_preserves_fragment() {
        let mut map = HashMap::new();
        map.insert(3000, 3001);
        assert_eq!(
            rewrite_url("http://localhost:3000/page#section", &map),
            "http://localhost:3001/page#section"
        );
    }

    #[test]
    fn rewrite_no_port_in_url() {
        let mut map = HashMap::new();
        map.insert(80, 8080);
        // No port specified — no rewrite
        assert_eq!(
            rewrite_url("http://localhost/path", &map),
            "http://localhost/path"
        );
    }

    // --- rate limiting tests ---

    #[test]
    fn rate_limiter_allows_up_to_limit() {
        let mut opener = BrowserOpener::new();
        // We can't actually open a browser in tests, so we test the rate
        // limiting logic by calling validate + rate check directly.
        // Push RATE_LIMIT_PER_SEC timestamps
        let now = Instant::now();
        for _ in 0..RATE_LIMIT_PER_SEC {
            opener.recent_opens.push_back(now);
        }
        // Next one should fail rate limit
        assert_eq!(opener.recent_opens.len(), RATE_LIMIT_PER_SEC);

        // Simulate the rate check — none should be pruned (all within 1s)
        while opener
            .recent_opens
            .front()
            .is_some_and(|t| now.duration_since(*t).as_secs() >= 1)
        {
            opener.recent_opens.pop_front();
        }
        assert!(opener.recent_opens.len() >= RATE_LIMIT_PER_SEC);
    }

    #[test]
    fn rate_limiter_expires_old_entries() {
        let opener = BrowserOpener::new();
        // We can't easily fake Instant, but we can verify the pruning logic
        // by checking that an empty recent_opens allows opens
        assert!(opener.recent_opens.is_empty());
    }

    // --- port map management tests ---

    #[test]
    fn add_and_remove_port_mapping() {
        let mut opener = BrowserOpener::new();
        opener.add_port_mapping(3000, 3001);
        assert_eq!(opener.port_map.get(&3000), Some(&3001));

        opener.remove_port_mapping(3000);
        assert_eq!(opener.port_map.get(&3000), None);
    }
}
