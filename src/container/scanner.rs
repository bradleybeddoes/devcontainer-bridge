//! `/proc/net/tcp` parser for detecting listening ports inside a container.
//!
//! Scans `/proc/net/tcp` and `/proc/net/tcp6` to find sockets in the
//! `TCP_LISTEN` state (hex state `0A`). Optionally resolves process names
//! by walking `/proc/{pid}/fd` for matching socket inodes.

use std::collections::{HashMap, HashSet};
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
/// field index 3 (state, `0A` = LISTEN). Field index 7 is the owning uid and
/// field index 9 is the inode.
fn parse_proc_net_tcp(content: &str) -> Vec<ListeningSocket> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (_, port_hex) = fields[1].rsplit_once(':')?;
            Some(ListeningSocket {
                port: u16::from_str_radix(port_hex, 16).ok()?,
                uid: fields[7].parse().ok()?,
                inode: fields[9].parse().ok()?,
            })
        })
        .collect()
}

/// A listening socket as described by a `/proc/net/tcp` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListeningSocket {
    port: u16,
    uid: u32,
    inode: u64,
}

/// Read this process's effective uid from `/proc/self/status`.
///
/// Returns `None` when the field cannot be read, which disables the ownership
/// pre-check rather than skipping work that might have succeeded.
fn effective_uid(proc_path: &Path) -> Option<u32> {
    std::fs::read_to_string(proc_path.join("self/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1)?.parse().ok())
}

/// Resolve owning process names for a set of socket inodes in one `/proc` walk.
///
/// Walks `/proc/{pid}/fd/` once, matching every symlink against the wanted
/// inode set, and stops early once all of them are found. This is best-effort:
/// inodes owned by another user or another PID namespace simply stay absent.
///
/// Synchronous on purpose. Every `tokio::fs` call is a `spawn_blocking` round
/// trip, and one walk of a small container is ~740 of them; the caller runs
/// this whole function inside a single blocking task instead.
fn resolve_inodes(proc_path: &Path, wanted: &HashSet<u64>) -> HashMap<u64, (String, u32)> {
    let mut found: HashMap<u64, (String, u32)> = HashMap::new();
    let Ok(proc_dir) = std::fs::read_dir(proc_path) else {
        return found;
    };

    for entry in proc_dir.flatten() {
        if found.len() == wanted.len() {
            break;
        }
        let pid_name = entry.file_name();
        let Some(pid_str) = pid_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(proc_path.join(pid_str).join("fd")) else {
            continue;
        };

        let mut name: Option<String> = None;
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = link
                .to_str()
                .and_then(|l| l.strip_prefix("socket:["))
                .and_then(|l| l.strip_suffix(']'))
                .and_then(|l| l.parse::<u64>().ok())
            else {
                continue;
            };
            if !wanted.contains(&inode) || found.contains_key(&inode) {
                continue;
            }
            let comm = name.get_or_insert_with(|| {
                std::fs::read_to_string(proc_path.join(pid_str).join("comm"))
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default()
            });
            found.insert(inode, (comm.clone(), pid));
        }
    }
    found
}

/// Scan for listening TCP ports inside the container.
///
/// Reads `/proc/net/tcp` and `/proc/net/tcp6`, parses for `LISTEN` state
/// sockets, excludes ports in `exclude_ports`, and resolves process names for
/// ports not already in `resolved`.
///
/// # Process name resolution
///
/// Resolution walks every `/proc/{pid}/fd`, so it is by far the most expensive
/// part of a scan. Two filters keep it off the steady-state path:
///
/// * Ports in `resolved` are skipped. The caller sends a process name only when
///   it first forwards a port, so re-resolving a forwarded port is pure waste.
/// * Sockets owned by another uid are skipped when this process is not root,
///   because `/proc/{pid}/fd` of another user's process is unreadable and the
///   walk can only ever fail.
///
/// Whatever survives both filters is resolved in a single walk inside one
/// blocking task.
///
/// # Arguments
///
/// * `proc_path` — Root path to `/proc` (allows injection for testing).
/// * `exclude_ports` — Ports to exclude from results (e.g., control/data ports).
/// * `resolved` — Ports whose process name the caller already has.
///
/// # Errors
///
/// Returns [`ScanError`] if neither `/proc/net/tcp` nor `/proc/net/tcp6` can
/// be read. Individual file read failures are tolerated.
pub async fn scan_listening_ports(
    proc_path: &Path,
    exclude_ports: &HashSet<u16>,
    resolved: &HashSet<u16>,
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
    let sockets: Vec<ListeningSocket> = [&tcp_content, &tcp6_content]
        .into_iter()
        .filter_map(|c| c.as_ref().ok())
        .flat_map(|text| parse_proc_net_tcp(text))
        .filter(|s| !exclude_ports.contains(&s.port) && seen_ports.insert(s.port))
        .collect();

    let euid = effective_uid(proc_path);
    let wanted: HashSet<u64> = sockets
        .iter()
        .filter(|s| !resolved.contains(&s.port))
        .filter(|s| euid.is_none_or(|euid| euid == 0 || euid == s.uid))
        .map(|s| s.inode)
        .collect();

    let found = if wanted.is_empty() {
        HashMap::new()
    } else {
        let proc_path = proc_path.to_owned();
        // A panic in the walk degrades to "no names resolved", which resolution
        // is already documented to allow.
        tokio::task::spawn_blocking(move || resolve_inodes(&proc_path, &wanted))
            .await
            .unwrap_or_default()
    };

    Ok(sockets
        .into_iter()
        .map(|s| {
            let resolved = found.get(&s.inode);
            ListeningPort {
                port: s.port,
                process_name: resolved
                    .map(|(name, _)| name.clone())
                    .filter(|n| !n.is_empty()),
                pid: resolved.map(|(_, pid)| *pid),
            }
        })
        .collect())
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
        let ports: Vec<u16> = results.iter().map(|s| s.port).collect();
        // 0x1F90 = 8080, 0x4B59 = 19289, 0x0050 = 80
        // Line 2 is state 01 (ESTABLISHED), should be excluded
        assert_eq!(ports, vec![8080, 19289, 80]);
    }

    #[test]
    fn parse_tcp6_extracts_listening_ports() {
        let results = parse_proc_net_tcp(PROC_NET_TCP6_FIXTURE);
        let ports: Vec<u16> = results.iter().map(|s| s.port).collect();
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
        assert_eq!(results[0].port, 8080);
    }

    /// Build a fake `/proc` holding one LISTEN socket owned by `uid`, plus a
    /// process whose fd table points at that socket's inode.
    #[cfg(unix)]
    fn fake_proc(uid: u32, inode: u64, port: u16, euid: u32) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("net")).unwrap();
        std::fs::write(
            root.join("net/tcp"),
            format!(
                "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n                    0: 00000000:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000  {uid}        0 {inode} 1 0 100 0 0 10 0\n"
            ),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("self")).unwrap();
        std::fs::write(
            root.join("self/status"),
            format!("Name:\tdbr\nUid:\t{euid}\t{euid}\t{euid}\t{euid}\n"),
        )
        .unwrap();
        let fd_dir = root.join("4242/fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::fs::write(root.join("4242/comm"), "target-process\n").unwrap();
        std::os::unix::fs::symlink(format!("socket:[{inode}]"), fd_dir.join("3")).unwrap();
        tmp
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolution_is_skipped_for_known_ports_and_foreign_uids() {
        let empty = HashSet::new();

        // Baseline: an unknown port owned by this uid resolves.
        let tmp = fake_proc(1000, 555, 8080, 1000);
        let ports = scan_listening_ports(tmp.path(), &empty, &empty)
            .await
            .unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].process_name.as_deref(), Some("target-process"));
        assert_eq!(ports[0].pid, Some(4242));

        // Already forwarded: the caller has the name, so no walk happens.
        let known: HashSet<u16> = [8080].into_iter().collect();
        let ports = scan_listening_ports(tmp.path(), &empty, &known)
            .await
            .unwrap();
        assert_eq!(ports.len(), 1, "the port is still reported");
        assert_eq!(
            ports[0].process_name, None,
            "a forwarded port must not be re-resolved"
        );

        // Foreign uid while unprivileged: /proc/<pid>/fd would be unreadable.
        let tmp = fake_proc(0, 556, 2222, 1000);
        let ports = scan_listening_ports(tmp.path(), &empty, &empty)
            .await
            .unwrap();
        assert_eq!(ports[0].process_name, None, "root-owned socket skipped");

        // Same socket, but running as root: the pre-check must not fire.
        let tmp = fake_proc(0, 557, 2222, 0);
        let ports = scan_listening_ports(tmp.path(), &empty, &empty)
            .await
            .unwrap();
        assert_eq!(
            ports[0].process_name.as_deref(),
            Some("target-process"),
            "root resolves sockets of any uid"
        );
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

        let results = scan_listening_ports(tmp.path(), &exclude, &HashSet::new())
            .await
            .unwrap();
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

        let results = scan_listening_ports(tmp.path(), &HashSet::new(), &HashSet::new())
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
        let result = scan_listening_ports(tmp.path(), &HashSet::new(), &HashSet::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn scan_tolerates_missing_tcp6() {
        let tmp = tempfile::tempdir().unwrap();
        let net_dir = tmp.path().join("net");
        std::fs::create_dir_all(&net_dir).unwrap();
        std::fs::write(net_dir.join("tcp"), PROC_NET_TCP_FIXTURE).unwrap();
        // No tcp6 file

        let results = scan_listening_ports(tmp.path(), &HashSet::new(), &HashSet::new())
            .await
            .unwrap();
        assert!(!results.is_empty());
    }
}
