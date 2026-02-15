//! `/proc/net/tcp` parser for detecting listening ports inside a container.
//!
//! Scans `/proc/net/tcp` and `/proc/net/tcp6` to find sockets in the
//! `TCP_LISTEN` state (hex state `0A`). Optionally resolves process names
//! by walking `/proc/{pid}/fd` for matching socket inodes.

use std::collections::HashSet;
use std::path::Path;

use thiserror::Error;
use tokio::fs;

/// Errors that can occur during port scanning.
#[derive(Debug, Error)]
pub enum ScanError {
    /// Failed to read a `/proc` file.
    #[error("failed to read {path}: {source}")]
    ReadFile {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// A port detected as listening inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListeningPort {
    /// The TCP port number.
    pub port: u16,
    /// Name of the process listening on this port, if resolvable.
    pub process_name: Option<String>,
    /// PID of the listening process, if resolvable.
    pub pid: Option<u32>,
}

/// Parse a single `/proc/net/tcp` (or tcp6) file content and return listening ports.
///
/// Each line after the header has fields separated by whitespace. The format is:
/// ```text
///   sl  local_address rem_address   st tx_queue:rx_queue ...
/// ```
/// We care about field index 1 (local_address as `HEX_IP:HEX_PORT`) and
/// field index 3 (state, `0A` = LISTEN). Field index 9 is the inode.
fn parse_proc_net_tcp(content: &str) -> Vec<(u16, u64)> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (_, port_hex) = fields[1].rsplit_once(':')?;
            let port = u16::from_str_radix(port_hex, 16).ok()?;
            let inode: u64 = fields[9].parse().ok()?;
            Some((port, inode))
        })
        .collect()
}

/// Attempt to resolve the process name and PID for a given socket inode.
///
/// Walks `/proc/{pid}/fd/` directories looking for symlinks to `socket:[{inode}]`.
/// This is best-effort — it may fail due to permission restrictions or race
/// conditions, which is acceptable.
///
/// # Performance
///
/// Complexity is O(processes * file_descriptors) which can be slow with
/// many processes. In typical devcontainers (<100 processes), this
/// completes in under 10ms. The function is called once per detected
/// port per scan cycle, not per connection.
async fn resolve_process_for_inode(proc_path: &Path, inode: u64) -> Option<(String, u32)> {
    let target = format!("socket:[{inode}]");
    let mut proc_dir = match fs::read_dir(proc_path).await {
        Ok(d) => d,
        Err(_) => return None,
    };

    while let Ok(Some(entry)) = proc_dir.next_entry().await {
        let pid_str = entry.file_name();
        let pid_str = pid_str.to_string_lossy();
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let fd_dir = proc_path.join(&*pid_str).join("fd");
        let mut fds = match fs::read_dir(&fd_dir).await {
            Ok(d) => d,
            Err(_) => continue,
        };

        while let Ok(Some(fd_entry)) = fds.next_entry().await {
            let link = match fs::read_link(fd_entry.path()).await {
                Ok(l) => l,
                Err(_) => continue,
            };
            if link.to_string_lossy() == target {
                // Found the process — read its comm
                let comm_path = proc_path.join(&*pid_str).join("comm");
                let name = match fs::read_to_string(&comm_path).await {
                    Ok(s) => s.trim().to_owned(),
                    Err(_) => return Some((String::new(), pid)),
                };
                return Some((name, pid));
            }
        }
    }

    None
}

/// Scan for listening TCP ports inside the container.
///
/// Reads `/proc/net/tcp` and `/proc/net/tcp6`, parses for `LISTEN` state
/// sockets, excludes ports in `exclude_ports`, and optionally resolves
/// process names.
///
/// # Arguments
///
/// * `proc_path` — Root path to `/proc` (allows injection for testing).
/// * `exclude_ports` — Ports to exclude from results (e.g., control/data ports).
///
/// # Errors
///
/// Returns [`ScanError`] if neither `/proc/net/tcp` nor `/proc/net/tcp6` can
/// be read. Individual file read failures are tolerated.
pub async fn scan_listening_ports(
    proc_path: &Path,
    exclude_ports: &HashSet<u16>,
) -> Result<Vec<ListeningPort>, ScanError> {
    let tcp_path = proc_path.join("net/tcp");
    let tcp6_path = proc_path.join("net/tcp6");

    let tcp_content = fs::read_to_string(&tcp_path).await;
    let tcp6_content = fs::read_to_string(&tcp6_path).await;

    // At least one must succeed
    if let (Err(e), Err(_)) = (&tcp_content, &tcp6_content) {
        return Err(ScanError::ReadFile {
            path: tcp_path.to_string_lossy().into_owned(),
            source: std::io::Error::new(e.kind(), e.to_string()),
        });
    }

    let mut seen_ports = HashSet::new();
    let port_inodes: Vec<(u16, u64)> = [&tcp_content, &tcp6_content]
        .into_iter()
        .filter_map(|c| c.as_ref().ok())
        .flat_map(|text| parse_proc_net_tcp(text))
        .filter(|(port, _)| !exclude_ports.contains(port) && seen_ports.insert(*port))
        .collect();

    let mut results = Vec::with_capacity(port_inodes.len());
    for (port, inode) in port_inodes {
        let resolved = resolve_process_for_inode(proc_path, inode).await;
        results.push(ListeningPort {
            port,
            process_name: resolved
                .as_ref()
                .map(|(name, _)| name.clone())
                .filter(|n| !n.is_empty()),
            pid: resolved.map(|(_, pid)| pid),
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_NET_TCP_FIXTURE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0
   1: 0100007F:4B59 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 23456 1 0000000000000000 100 0 0 10 0
   2: 0100007F:C3A8 AC110002:01BB 01 00000000:00000000 02:0000009C 00000000  1000        0 34567 2 0000000000000000 20 4 30 10 -1
   3: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 45678 1 0000000000000000 100 0 0 10 0
";

    const PROC_NET_TCP6_FIXTURE: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:23F1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 56789 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn parse_tcp_extracts_listening_ports() {
        let results = parse_proc_net_tcp(PROC_NET_TCP_FIXTURE);
        let ports: Vec<u16> = results.iter().map(|(p, _)| *p).collect();
        // 0x1F90 = 8080, 0x4B59 = 19289, 0x0050 = 80
        // Line 2 is state 01 (ESTABLISHED), should be excluded
        assert_eq!(ports, vec![8080, 19289, 80]);
    }

    #[test]
    fn parse_tcp6_extracts_listening_ports() {
        let results = parse_proc_net_tcp(PROC_NET_TCP6_FIXTURE);
        let ports: Vec<u16> = results.iter().map(|(p, _)| *p).collect();
        // 0x1F90 = 8080, 0x23F1 = 9201
        assert_eq!(ports, vec![8080, 9201]);
    }

    #[test]
    fn parse_empty_content() {
        let results = parse_proc_net_tcp("");
        assert!(results.is_empty());
    }

    #[test]
    fn parse_header_only() {
        let results = parse_proc_net_tcp(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        );
        assert!(results.is_empty());
    }

    #[test]
    fn parse_malformed_line_skipped() {
        let content = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: garbage_data 0A
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0
";
        let results = parse_proc_net_tcp(content);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 8080);
    }

    #[tokio::test]
    async fn scan_excludes_specified_ports() {
        // Create a temporary proc-like directory structure
        let tmp = tempfile::tempdir().unwrap();
        let net_dir = tmp.path().join("net");
        std::fs::create_dir_all(&net_dir).unwrap();
        std::fs::write(net_dir.join("tcp"), PROC_NET_TCP_FIXTURE).unwrap();

        let mut exclude = HashSet::new();
        exclude.insert(80);
        exclude.insert(19289);

        let results = scan_listening_ports(tmp.path(), &exclude).await.unwrap();
        let ports: Vec<u16> = results.iter().map(|lp| lp.port).collect();
        assert_eq!(ports, vec![8080]);
    }

    #[tokio::test]
    async fn scan_deduplicates_across_tcp_and_tcp6() {
        let tmp = tempfile::tempdir().unwrap();
        let net_dir = tmp.path().join("net");
        std::fs::create_dir_all(&net_dir).unwrap();
        std::fs::write(net_dir.join("tcp"), PROC_NET_TCP_FIXTURE).unwrap();
        std::fs::write(net_dir.join("tcp6"), PROC_NET_TCP6_FIXTURE).unwrap();

        let results = scan_listening_ports(tmp.path(), &HashSet::new())
            .await
            .unwrap();
        let ports: Vec<u16> = results.iter().map(|lp| lp.port).collect();
        // 8080 appears in both tcp and tcp6, should only appear once
        // Total: 8080, 19289, 80 from tcp + 9201 from tcp6 (8080 deduped)
        assert_eq!(ports, vec![8080, 19289, 80, 9201]);
    }

    #[tokio::test]
    async fn scan_fails_when_no_proc_files() {
        let tmp = tempfile::tempdir().unwrap();
        let result = scan_listening_ports(tmp.path(), &HashSet::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn scan_tolerates_missing_tcp6() {
        let tmp = tempfile::tempdir().unwrap();
        let net_dir = tmp.path().join("net");
        std::fs::create_dir_all(&net_dir).unwrap();
        std::fs::write(net_dir.join("tcp"), PROC_NET_TCP_FIXTURE).unwrap();
        // No tcp6 file

        let results = scan_listening_ports(tmp.path(), &HashSet::new())
            .await
            .unwrap();
        assert!(!results.is_empty());
    }
}
