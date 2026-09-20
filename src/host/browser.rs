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

    /// The browser command did not exit within the time limit.
    #[error("browser command timed out after {}s", BROWSER_TIMEOUT.as_secs())]
    Timeout,
}

/// Time limit for the browser process to exit.
const BROWSER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        // The caller bounds this wait with a timeout, and dropping the future
        // would otherwise leave a hung browser running with nothing tracking it.
        .kill_on_drop(true)
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
        let (rewritten, cmd) = self.prepare(url)?;
        launch(&rewritten, cmd.as_deref()).await
    }

    /// Validate, rate-limit and rewrite a URL, returning it with the command to run.
    ///
    /// Separated from [`launch`] so a caller can release the browser lock before
    /// waiting on the browser process. Forward, unforward and container cleanup
    /// all take that same lock, so holding it across the wait would block port
    /// forwarding for every container behind one slow browser.
    ///
    /// # Errors
    ///
    /// Returns [`BrowserError`] if validation fails or the rate limit is exceeded.
    pub fn prepare(&mut self, url: &str) -> Result<(String, Option<String>), BrowserError> {
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
        Ok((rewritten, self.browser_cmd.clone()))
    }
}

/// Run the browser command for an already-prepared URL.
///
/// Holds no lock, so it is safe to await after releasing the browser mutex.
/// A browser that never exits fails after [`BROWSER_TIMEOUT`] rather than
/// stalling its caller indefinitely.
///
/// # Errors
///
/// Returns [`BrowserError`] if the command fails to spawn, exits non-zero, or
/// does not exit within the time limit.
pub async fn launch(url: &str, browser_cmd: Option<&str>) -> Result<(), BrowserError> {
    tokio::time::timeout(BROWSER_TIMEOUT, open_in_browser(url, browser_cmd))
        .await
        .map_err(|_| BrowserError::Timeout)??;
    info!(url, "opened URL in browser");
    Ok(())
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
        // Old entries should be pruned, allowing new opens. Check `prepare`
        // rather than `open`: the rate limiter is what is under test, and with
        // a paused clock the runtime would advance straight to `launch`'s
        // timeout while waiting on the real subprocess.
        let result = opener.prepare("http://localhost:8080");
        assert!(result.is_ok());
    }

    // --- browser timeout tests ---

    /// A sleep duration unique to this test process, so a stray left behind
    /// by an earlier failed run is never mistaken for this run's child.
    #[cfg(unix)]
    fn unique_marker(offset: u32) -> String {
        (20_000 + (std::process::id() % 10_000) * 3 + offset).to_string()
    }

    /// A browser command that never exits. `exec` keeps the pid, so killing
    /// the direct child really stops it; without it `sleep` would survive as
    /// an orphaned grandchild.
    #[cfg(unix)]
    fn hanging_browser(marker: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hang");
        std::fs::write(&script, format!("#!/bin/sh\nexec sleep {marker}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script)
    }

    /// `ps` rather than `pgrep`, whose `-f` does not reliably match an
    /// `exec`ed child.
    ///
    /// Must not be called before the browser child is spawned: running any
    /// other child first leaves that spawn's future hanging forever.
    #[cfg(unix)]
    fn browser_running(marker: &str) -> bool {
        let out = std::process::Command::new("ps")
            .arg("-ax")
            .arg("-o")
            .arg("command=")
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout).contains(&format!("sleep {marker}"))
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn launch_times_out_when_the_browser_never_exits() {
        let (_dir, script) = hanging_browser(&unique_marker(0));
        let result = launch("http://localhost:8080", script.to_str()).await;
        assert!(matches!(result, Err(BrowserError::Timeout)), "{result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timing_out_kills_the_browser_instead_of_orphaning_it() {
        let marker = unique_marker(1);
        let (_dir, script) = hanging_browser(&marker);

        // A real clock, not `start_paused`: killing and reaping the child
        // takes wall time, and that it happens at all is the point here.
        // Scoped rather than `drop(fut)`: `tokio::pin!` yields a
        // `Pin<&mut F>`, so dropping that drops a reference, not the future.
        {
            let fut = open_in_browser("http://localhost:8080", script.to_str());
            tokio::pin!(fut);
            let outcome = tokio::time::timeout(Duration::from_millis(300), &mut fut).await;
            assert!(outcome.is_err(), "browser exited on its own: {outcome:?}");

            // Poll rather than assert once: fork, exec and `ps` can each take
            // longer than the timeout window on a loaded machine.
            let mut started = false;
            for _ in 0..100 {
                if browser_running(&marker) {
                    started = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(started, "browser never started");
        }

        for _ in 0..100 {
            if !browser_running(&marker) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Best effort, so a failure here does not poison the next run.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(format!("sleep {marker}"))
            .status();
        panic!("browser survived the dropped future");
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
