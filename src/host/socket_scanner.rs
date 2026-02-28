//! Scans the host filesystem for Unix domain sockets matching configured glob patterns.
//!
//! The scanner periodically expands glob patterns, verifies matches are actual
//! Unix sockets (not regular files or symlinks), and tracks their lifecycle.
//! New sockets trigger `SocketForward` messages; removed sockets trigger
//! `SocketUnforward` messages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::{debug, warn};

/// Information about a discovered Unix domain socket.
#[derive(Debug, Clone)]
pub struct SocketInfo {
    /// Unique identifier for this socket forward.
    pub socket_id: String,
    /// Absolute path on the host filesystem.
    pub host_path: PathBuf,
    /// Path to create inside the container.
    pub container_path: String,
    /// When this socket was first discovered.
    pub discovered_at: Instant,
}

/// Scans configured directories for Unix domain sockets.
///
/// Expands glob patterns on each scan, verifies that matches are actual Unix
/// sockets (using `lstat` to avoid following symlinks), and tracks socket
/// lifecycle (new, existing, removed).
pub struct SocketScanner {
    /// Glob patterns to expand.
    watch_paths: Vec<String>,
    /// Optional prefix to rewrite container paths.
    container_path_prefix: Option<String>,
    /// Currently known sockets, keyed by host path.
    known: HashMap<PathBuf, SocketInfo>,
    /// Maximum number of socket forwards allowed.
    max_socket_forwards: usize,
}

impl SocketScanner {
    /// Creates a new socket scanner.
    ///
    /// # Arguments
    ///
    /// * `watch_paths` — Glob patterns to expand when scanning (e.g., `/tmp/.ssh-*/*`).
    /// * `container_path_prefix` — If `Some`, replaces the directory portion of
    ///   discovered socket paths when computing the container path.
    /// * `max_socket_forwards` — Maximum number of sockets to track simultaneously.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use dbr::host::socket_scanner::SocketScanner;
    ///
    /// let scanner = SocketScanner::new(
    ///     vec!["/tmp/.ssh-*/*".to_string()],
    ///     Some("/run/host-sockets".to_string()),
    ///     32,
    /// );
    /// ```
    pub fn new(
        watch_paths: Vec<String>,
        container_path_prefix: Option<String>,
        max_socket_forwards: usize,
    ) -> Self {
        Self {
            watch_paths,
            container_path_prefix,
            known: HashMap::new(),
            max_socket_forwards,
        }
    }

    /// Scans all configured glob patterns and returns newly discovered and removed sockets.
    ///
    /// Each glob pattern is expanded and each match is verified to be an actual Unix
    /// domain socket (not a regular file or symlink). The scanner tracks state across
    /// calls so that:
    /// - **Newly found** sockets are those present in this scan but absent from previous scans.
    /// - **Removed** sockets are those absent from this scan but present in previous scans.
    ///
    /// The total number of tracked sockets is capped at `max_socket_forwards`. Once the
    /// limit is reached, additional new sockets are ignored until existing ones are removed.
    ///
    /// # Returns
    ///
    /// A tuple `(newly_found, removed)` where each element is a `Vec<SocketInfo>`.
    ///
    /// # Errors
    ///
    /// This method does not return errors. Invalid glob patterns and inaccessible paths
    /// are logged as warnings and skipped.
    pub fn scan(&mut self) -> (Vec<SocketInfo>, Vec<SocketInfo>) {
        let mut current_paths = HashMap::new();

        for pattern in &self.watch_paths {
            let entries = match glob::glob(pattern) {
                Ok(entries) => entries,
                Err(e) => {
                    warn!(pattern = %pattern, error = %e, "invalid glob pattern, skipping");
                    continue;
                }
            };

            for entry in entries {
                let path = match entry {
                    Ok(path) => path,
                    Err(e) => {
                        debug!(error = %e, "glob entry error, skipping");
                        continue;
                    }
                };

                if is_unix_socket(&path) {
                    current_paths.insert(path.clone(), path);
                }
            }
        }

        // Find removed sockets: in known but not in current scan.
        let removed: Vec<SocketInfo> = self
            .known
            .keys()
            .filter(|path| !current_paths.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|path| self.known.remove(&path))
            .collect();

        // Find newly discovered sockets: in current scan but not in known.
        let mut newly_found = Vec::new();
        for path in current_paths.keys() {
            if self.known.contains_key(path) {
                continue;
            }

            if self.known.len() >= self.max_socket_forwards {
                warn!(
                    max = self.max_socket_forwards,
                    path = %path.display(),
                    "socket forward limit reached, ignoring new socket"
                );
                break;
            }

            let container_path = self.compute_container_path(path);
            let info = SocketInfo {
                socket_id: uuid::Uuid::new_v4().to_string(),
                host_path: path.clone(),
                container_path,
                discovered_at: Instant::now(),
            };

            newly_found.push(info.clone());
            self.known.insert(path.clone(), info);
        }

        (newly_found, removed)
    }

    /// Computes the container-side path for a discovered host socket.
    ///
    /// If `container_path_prefix` is set, the directory portion of `host_path` is
    /// replaced with the prefix while keeping the filename. If no prefix is configured,
    /// the host path is used as-is.
    ///
    /// # Examples
    ///
    /// With prefix `/run/host-sockets`:
    /// - `/tmp/.ssh-abc/agent.123` → `/run/host-sockets/agent.123`
    ///
    /// Without prefix:
    /// - `/tmp/.ssh-abc/agent.123` → `/tmp/.ssh-abc/agent.123`
    fn compute_container_path(&self, host_path: &Path) -> String {
        match &self.container_path_prefix {
            Some(prefix) => {
                let filename = host_path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let prefix_path = Path::new(prefix);
                prefix_path.join(&filename).to_string_lossy().into_owned()
            }
            None => host_path.to_string_lossy().into_owned(),
        }
    }

    /// Returns all currently known sockets.
    pub fn known_sockets(&self) -> Vec<&SocketInfo> {
        self.known.values().collect()
    }
}

/// Checks whether the given path is an actual Unix domain socket.
///
/// Uses `symlink_metadata` (equivalent to `lstat`) to avoid following symlinks.
/// A symlink pointing to a socket will return `false` — only actual socket files
/// are detected.
///
/// # Arguments
///
/// * `path` — The filesystem path to check.
///
/// # Returns
///
/// `true` if the path exists and is a Unix domain socket; `false` otherwise.
pub fn is_unix_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type().is_socket(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    /// Helper to create a Unix socket in the given directory with the given name.
    fn create_socket(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let _listener = UnixListener::bind(&path).expect("failed to bind Unix socket");
        // Drop the listener — the socket file remains on disk.
        path
    }

    #[test]
    fn test_scan_detects_new_socket() {
        let tmp = TempDir::new().unwrap();
        let sock_path = create_socket(tmp.path(), "test.sock");

        let glob_pattern = format!("{}/*.sock", tmp.path().display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 32);

        let (found, removed) = scanner.scan();
        assert_eq!(found.len(), 1);
        assert!(removed.is_empty());
        assert_eq!(found[0].host_path, sock_path);
    }

    #[test]
    fn test_scan_detects_removal() {
        let tmp = TempDir::new().unwrap();
        let sock_path = create_socket(tmp.path(), "test.sock");

        let glob_pattern = format!("{}/*.sock", tmp.path().display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 32);

        // First scan: detect the socket.
        let (found, removed) = scanner.scan();
        assert_eq!(found.len(), 1);
        assert!(removed.is_empty());

        // Remove the socket file.
        std::fs::remove_file(&sock_path).unwrap();

        // Second scan: detect the removal.
        let (found, removed) = scanner.scan();
        assert!(found.is_empty());
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].host_path, sock_path);
    }

    #[test]
    fn test_symlink_not_detected() {
        let tmp = TempDir::new().unwrap();
        let sock_dir = tmp.path().join("socks");
        let link_dir = tmp.path().join("links");
        std::fs::create_dir_all(&sock_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();

        let sock_path = create_socket(&sock_dir, "real.sock");

        // Create a symlink to the socket in the watched directory.
        let link_path = link_dir.join("link.sock");
        std::os::unix::fs::symlink(&sock_path, &link_path).unwrap();

        // Only watch the links directory.
        let glob_pattern = format!("{}/*.sock", link_dir.display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 32);

        let (found, _removed) = scanner.scan();
        assert!(
            found.is_empty(),
            "symlink to a socket should not be detected"
        );
    }

    #[test]
    fn test_regular_file_not_detected() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("not-a-socket.sock");
        std::fs::write(&file_path, b"regular file").unwrap();

        let glob_pattern = format!("{}/*.sock", tmp.path().display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 32);

        let (found, _removed) = scanner.scan();
        assert!(
            found.is_empty(),
            "regular file should not be detected as a socket"
        );
    }

    #[test]
    fn test_multiple_globs_multiple_sockets() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();

        let _sock1 = create_socket(tmp1.path(), "a.sock");
        let _sock2 = create_socket(tmp1.path(), "b.sock");
        let _sock3 = create_socket(tmp2.path(), "c.sock");

        let patterns = vec![
            format!("{}/*.sock", tmp1.path().display()),
            format!("{}/*.sock", tmp2.path().display()),
        ];
        let mut scanner = SocketScanner::new(patterns, None, 32);

        let (found, removed) = scanner.scan();
        assert_eq!(found.len(), 3);
        assert!(removed.is_empty());
    }

    #[test]
    fn test_container_path_rewrite() {
        let tmp = TempDir::new().unwrap();
        let _sock = create_socket(tmp.path(), "agent.123");

        let glob_pattern = format!("{}/*", tmp.path().display());
        let mut scanner = SocketScanner::new(
            vec![glob_pattern],
            Some("/run/host-sockets".to_string()),
            32,
        );

        let (found, _) = scanner.scan();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].container_path, "/run/host-sockets/agent.123");
    }

    #[test]
    fn test_container_path_no_prefix() {
        let tmp = TempDir::new().unwrap();
        let sock_path = create_socket(tmp.path(), "agent.456");

        let glob_pattern = format!("{}/*", tmp.path().display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 32);

        let (found, _) = scanner.scan();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].container_path, sock_path.to_string_lossy());
    }

    #[test]
    fn test_empty_watch_paths() {
        let mut scanner = SocketScanner::new(vec![], None, 32);

        let (found, removed) = scanner.scan();
        assert!(found.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn test_max_socket_forwards_limit() {
        let tmp = TempDir::new().unwrap();

        // Create 5 sockets but allow only 3.
        for i in 0..5 {
            create_socket(tmp.path(), &format!("sock{i}.sock"));
        }

        let glob_pattern = format!("{}/*.sock", tmp.path().display());
        let mut scanner = SocketScanner::new(vec![glob_pattern], None, 3);

        let (found, _) = scanner.scan();
        assert_eq!(
            found.len(),
            3,
            "should only track up to max_socket_forwards"
        );
        assert_eq!(
            scanner.known_sockets().len(),
            3,
            "known sockets should respect the limit"
        );
    }
}
