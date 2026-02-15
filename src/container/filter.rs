//! Port filtering logic for the container daemon.
//!
//! Determines which detected listening ports should be forwarded to the host.
//! Supports exclude lists, include/allowlist mode, process name regex filtering,
//! and reading `forwardPorts` from `devcontainer.json`.

use std::collections::HashSet;
use std::path::Path;

use regex::Regex;
use tracing::{debug, warn};

use super::scanner::ListeningPort;

/// Decides which listening ports should be forwarded to the host.
///
/// Filtering rules are applied in the following order:
/// 1. **Exclude ports** — ports in this set are never forwarded.
/// 2. **Include ports** — if non-empty, only ports in this set are forwarded.
///    The include set is the union of `--include-ports` and `forwardPorts` from
///    `devcontainer.json`.
/// 3. **Exclude process** — if a regex is set, ports whose process name matches
///    are excluded.
///
/// The default (no filters configured) forwards all detected listening ports,
/// matching VS Code behavior.
#[derive(Debug)]
pub struct PortFilter {
    /// Ports that are never forwarded (e.g., `22,5432`).
    exclude_ports: HashSet<u16>,
    /// If non-empty, only these ports are forwarded (allowlist mode).
    include_ports: HashSet<u16>,
    /// Compiled regex for excluding ports by process name.
    exclude_process: Option<Regex>,
}

/// Errors that can occur when constructing a [`PortFilter`].
#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    /// The `--exclude-process` regex pattern is invalid.
    #[error("invalid --exclude-process regex: {0}")]
    InvalidRegex(#[from] regex::Error),
}

impl PortFilter {
    /// Create a new port filter from the given configuration.
    ///
    /// # Arguments
    ///
    /// * `exclude_ports` — Ports to never forward.
    /// * `include_ports` — If non-empty, only forward these ports (allowlist).
    /// * `exclude_process` — Optional regex pattern to exclude by process name.
    /// * `devcontainer_json_path` — Optional path to `devcontainer.json`; if it
    ///   contains `forwardPorts`, those are merged into the include set.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError::InvalidRegex`] if `exclude_process` is not a
    /// valid regex.
    pub fn new(
        exclude_ports: &[u16],
        include_ports: &[u16],
        exclude_process: Option<&str>,
        devcontainer_json_path: Option<&Path>,
    ) -> Result<Self, FilterError> {
        let exclude_ports: HashSet<u16> = exclude_ports.iter().copied().collect();

        let mut include_set: HashSet<u16> = include_ports.iter().copied().collect();

        // Merge forwardPorts from devcontainer.json if present
        if let Some(path) = devcontainer_json_path {
            match read_forward_ports(path) {
                Ok(ports) => {
                    if !ports.is_empty() {
                        debug!(
                            count = ports.len(),
                            "loaded forwardPorts from devcontainer.json"
                        );
                        include_set.extend(ports);
                    }
                }
                Err(e) => {
                    debug!(error = %e, "could not read forwardPorts from devcontainer.json");
                }
            }
        }

        let exclude_process = exclude_process.map(Regex::new).transpose()?;

        Ok(Self {
            exclude_ports,
            include_ports: include_set,
            exclude_process,
        })
    }

    /// Check whether a listening port should be forwarded.
    ///
    /// Returns `true` if the port passes all filter rules.
    pub fn should_forward(&self, port: &ListeningPort) -> bool {
        // Rule 1: excluded ports are never forwarded
        if self.exclude_ports.contains(&port.port) {
            debug!(port = port.port, "filtered out: excluded port");
            return false;
        }

        // Rule 2: if include set is active, port must be in it
        if !self.include_ports.is_empty() && !self.include_ports.contains(&port.port) {
            debug!(port = port.port, "filtered out: not in include list");
            return false;
        }

        // Rule 3: exclude by process name regex
        if let Some(ref regex) = self.exclude_process {
            if let Some(ref name) = port.process_name {
                if regex.is_match(name) {
                    debug!(port = port.port, process = %name, "filtered out: process regex match");
                    return false;
                }
            }
        }

        true
    }

    /// Filter a list of listening ports, returning only those that should be
    /// forwarded.
    pub fn filter(&self, ports: Vec<ListeningPort>) -> Vec<ListeningPort> {
        ports
            .into_iter()
            .filter(|p| self.should_forward(p))
            .collect()
    }
}

/// Read `forwardPorts` from a `devcontainer.json` file.
///
/// The `forwardPorts` field is expected to be an array of integers or strings
/// that can be parsed as port numbers. JSONC comments are not handled — only
/// standard JSON is supported.
fn read_forward_ports(path: &Path) -> Result<Vec<u16>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

    let Some(arr) = value.get("forwardPorts").and_then(|v| v.as_array()) else {
        if value.get("forwardPorts").is_some() {
            warn!("forwardPorts in devcontainer.json is not an array");
        }
        return Ok(Vec::new());
    };

    let ports = arr
        .iter()
        .filter_map(|item| {
            item.as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .or_else(|| item.as_str().and_then(|s| s.parse().ok()))
                .filter(|&p| p > 0)
        })
        .collect();

    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_port(port: u16, process_name: Option<&str>) -> ListeningPort {
        ListeningPort {
            port,
            process_name: process_name.map(|s| s.to_owned()),
            pid: Some(1000),
        }
    }

    #[test]
    fn default_filter_forwards_all() {
        let filter = PortFilter::new(&[], &[], None, None).unwrap();
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
        assert!(filter.should_forward(&make_port(3000, None)));
        assert!(filter.should_forward(&make_port(22, Some("sshd"))));
    }

    #[test]
    fn exclude_ports_blocks_specified() {
        let filter = PortFilter::new(&[22, 5432], &[], None, None).unwrap();
        assert!(!filter.should_forward(&make_port(22, Some("sshd"))));
        assert!(!filter.should_forward(&make_port(5432, Some("postgres"))));
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
    }

    #[test]
    fn include_ports_allowlist_mode() {
        let filter = PortFilter::new(&[], &[8080, 3000], None, None).unwrap();
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
        assert!(filter.should_forward(&make_port(3000, Some("python"))));
        assert!(!filter.should_forward(&make_port(9090, Some("java"))));
    }

    #[test]
    fn exclude_overrides_include() {
        // Port 8080 is in both exclude and include — exclude wins
        let filter = PortFilter::new(&[8080], &[8080, 3000], None, None).unwrap();
        assert!(!filter.should_forward(&make_port(8080, Some("node"))));
        assert!(filter.should_forward(&make_port(3000, Some("python"))));
    }

    #[test]
    fn exclude_process_regex() {
        let filter = PortFilter::new(&[], &[], Some("^(sshd|postgres)$"), None).unwrap();
        assert!(!filter.should_forward(&make_port(22, Some("sshd"))));
        assert!(!filter.should_forward(&make_port(5432, Some("postgres"))));
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
        // No process name — passes filter
        assert!(filter.should_forward(&make_port(9090, None)));
    }

    #[test]
    fn exclude_process_partial_match() {
        let filter = PortFilter::new(&[], &[], Some("ssh"), None).unwrap();
        assert!(!filter.should_forward(&make_port(22, Some("sshd"))));
        assert!(!filter.should_forward(&make_port(2222, Some("openssh-server"))));
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
    }

    #[test]
    fn invalid_regex_returns_error() {
        let result = PortFilter::new(&[], &[], Some("[invalid"), None);
        assert!(result.is_err());
    }

    #[test]
    fn filter_vec() {
        let filter = PortFilter::new(&[22], &[], None, None).unwrap();
        let ports = vec![
            make_port(22, Some("sshd")),
            make_port(8080, Some("node")),
            make_port(3000, Some("python")),
        ];
        let result = filter.filter(ports);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].port, 8080);
        assert_eq!(result[1].port, 3000);
    }

    #[test]
    fn devcontainer_json_forward_ports() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("devcontainer.json");
        std::fs::write(&json_path, r#"{"forwardPorts": [8080, 3000, "9090"]}"#).unwrap();

        let filter = PortFilter::new(&[], &[], None, Some(&json_path)).unwrap();
        // Should be in allowlist mode with those ports
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
        assert!(filter.should_forward(&make_port(3000, Some("python"))));
        assert!(filter.should_forward(&make_port(9090, Some("java"))));
        assert!(!filter.should_forward(&make_port(4000, Some("ruby"))));
    }

    #[test]
    fn devcontainer_json_merged_with_include() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("devcontainer.json");
        std::fs::write(&json_path, r#"{"forwardPorts": [3000]}"#).unwrap();

        // CLI includes 8080, devcontainer.json adds 3000
        let filter = PortFilter::new(&[], &[8080], None, Some(&json_path)).unwrap();
        assert!(filter.should_forward(&make_port(8080, None)));
        assert!(filter.should_forward(&make_port(3000, None)));
        assert!(!filter.should_forward(&make_port(4000, None)));
    }

    #[test]
    fn devcontainer_json_missing_file_is_ok() {
        let filter = PortFilter::new(
            &[],
            &[],
            None,
            Some(Path::new("/nonexistent/devcontainer.json")),
        )
        .unwrap();
        // Should forward everything (no filter active)
        assert!(filter.should_forward(&make_port(8080, None)));
    }

    #[test]
    fn devcontainer_json_no_forward_ports_key() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("devcontainer.json");
        std::fs::write(&json_path, r#"{"name": "test"}"#).unwrap();

        let filter = PortFilter::new(&[], &[], None, Some(&json_path)).unwrap();
        assert!(filter.should_forward(&make_port(8080, None)));
    }

    #[test]
    fn devcontainer_json_invalid_json_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("devcontainer.json");
        std::fs::write(&json_path, "not valid json {{{").unwrap();

        let filter = PortFilter::new(&[], &[], None, Some(&json_path)).unwrap();
        assert!(filter.should_forward(&make_port(8080, None)));
    }

    #[test]
    fn all_filters_combined() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("devcontainer.json");
        std::fs::write(&json_path, r#"{"forwardPorts": [8080, 3000, 5432]}"#).unwrap();

        let filter = PortFilter::new(
            &[5432],        // exclude postgres port
            &[],            // no CLI include (devcontainer.json provides)
            Some("^sshd$"), // exclude sshd
            Some(&json_path),
        )
        .unwrap();

        // 8080 — in forwardPorts, not excluded → forward
        assert!(filter.should_forward(&make_port(8080, Some("node"))));
        // 3000 — in forwardPorts, not excluded → forward
        assert!(filter.should_forward(&make_port(3000, Some("python"))));
        // 5432 — in forwardPorts but in exclude_ports → blocked
        assert!(!filter.should_forward(&make_port(5432, Some("postgres"))));
        // 9090 — not in forwardPorts → blocked by include filter
        assert!(!filter.should_forward(&make_port(9090, Some("java"))));
        // 8080 with sshd process — would match exclude regex → blocked
        assert!(!filter.should_forward(&make_port(8080, Some("sshd"))));
    }
}
