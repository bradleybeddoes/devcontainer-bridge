//! Host-side browser opening for `OpenUrl` messages from containers.
//!
//! Validates URLs (http/https only, length cap), rewrites `localhost` ports
//! when the container port is forwarded to a different host port, and opens
//! the URL in the host's default browser.

use std::collections::{HashMap, VecDeque};

use tokio::time::Instant;

use thiserror::Error;
use tracing::{debug, info, warn};

use crate::protocol;

/// Maximum number of browser opens per second.
const RATE_LIMIT_PER_SEC: usize = 5;

/// Errors that can occur when opening a URL in the host browser.
#[derive(Debug, Error)]
pub enum BrowserError {
    /// The URL failed validation (empty, bad scheme, too long, or control chars).
    #[error(transparent)]
    Validation(#[from] protocol::UrlValidationError),

    /// Too many URL open requests in a short period.
    #[error("rate limited: exceeded {RATE_LIMIT_PER_SEC} opens per second")]
    RateLimited,

    /// The browser open command failed.
    #[error("failed to open browser: {0}")]
    OpenFailed(String),
}

/// Validate that a URL is safe to open in the host browser.
///
/// Delegates to [`protocol::validate_open_url`] for consistent validation
/// across host and container sides.
///
/// # Errors
///
/// Returns [`BrowserError::Validation`] if the URL is invalid.
pub fn validate_url(url: &str) -> Result<(), BrowserError> {
    protocol::validate_open_url(url)?;
    Ok(())
}

/// Rewrite loopback ports in a URL using the port map.
///
/// Matches `localhost:PORT`, `127.0.0.1:PORT`, and `[::1]:PORT`. If `PORT`
/// is a key in `port_map`, the port is replaced with the mapped host port.
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

    for host_prefix in ["localhost:", "127.0.0.1:", "[::1]:"] {
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
async fn open_in_browser(url: &str, browser_cmd: Option<&str>) -> Result<(), BrowserError> {
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

    let status = tokio::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
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
    pub async fn open(&mut self, url: &str) -> Result<(), BrowserError> {
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

        open_in_browser(&rewritten, self.browser_cmd.as_deref()).await?;
        info!(url = rewritten.as_str(), "opened URL in browser");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // --- validate_url delegation tests ---

    #[test]
    fn validate_accepts_valid_urls() {
        assert!(validate_url("http://localhost:8080/path").is_ok());
        assert!(validate_url("https://example.com/auth/callback").is_ok());
    }

    #[test]
    fn validate_rejects_invalid_urls() {
        assert!(matches!(
            validate_url("ftp://example.com/file"),
            Err(BrowserError::Validation(_))
        ));
        assert!(matches!(validate_url(""), Err(BrowserError::Validation(_))));
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
    fn rewrite_ipv6_loopback_port() {
        let mut map = HashMap::new();
        map.insert(3000, 3001);
        assert_eq!(
            rewrite_url("http://[::1]:3000/callback", &map),
            "http://[::1]:3001/callback"
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

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_blocks_at_limit() {
        let mut opener = BrowserOpener::with_cmd(Some("true".to_string()));
        // Fill the sliding window to the limit
        for _ in 0..RATE_LIMIT_PER_SEC {
            opener.recent_opens.push_back(Instant::now());
        }
        // Next open should be rate limited
        let result = opener.open("http://localhost:8080").await;
        assert!(matches!(result, Err(BrowserError::RateLimited)));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_allows_after_window_expires() {
        let mut opener = BrowserOpener::with_cmd(Some("true".to_string()));
        // Fill the sliding window to the limit
        for _ in 0..RATE_LIMIT_PER_SEC {
            opener.recent_opens.push_back(Instant::now());
        }
        // Advance past the 1-second window
        tokio::time::advance(Duration::from_secs(2)).await;
        // Old entries should be pruned, allowing new opens
        let result = opener.open("http://localhost:8080").await;
        assert!(result.is_ok());
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
