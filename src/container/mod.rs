//! Container-side daemon for devcontainer-bridge.
//!
//! Runs inside a Linux devcontainer. Connects to the host daemon's control
//! channel, registers, scans for listening ports, and handles reverse data
//! connection requests.

pub mod browser;
pub mod data;
pub mod filter;
pub mod scanner;
#[cfg(unix)]
pub mod socket;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::control::{self, ControlConnection, ControlError};
use crate::protocol::{Message, Protocol};

use self::data::spawn_connect_handler;
use self::filter::PortFilter;
use self::scanner::{ListeningPort, ScanError};

/// Exponential backoff with reset, encapsulating retry delay state.
///
/// Used by the container daemon reconnection loop to progressively
/// slow down connection attempts on repeated failures, then reset
/// to the initial delay on a successful connection.
struct Backoff {
    /// Current delay before the next retry.
    current: Duration,
    /// Delay used on the first retry and after [`reset`].
    initial: Duration,
    /// Upper bound; [`escalate`] will not exceed this.
    max: Duration,
}

impl Backoff {
    /// Create a new backoff starting at `initial`, capped at `max`.
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            current: initial,
            initial,
            max,
        }
    }

    /// Double the current delay, capped at the maximum.
    fn escalate(&mut self) {
        self.current = (self.current * 2).min(self.max);
    }

    /// Reset the delay to the initial value after a successful connection.
    fn reset(&mut self) {
        self.current = self.initial;
    }

    /// Sleep for the current delay (escalating afterward), or return
    /// immediately if a shutdown signal arrives. Returns `true` on shutdown.
    async fn wait_or_shutdown(
        &mut self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(self.current) => {
                self.escalate();
                false
            }
            _ = shutdown.changed() => {
                info!("shutdown signal received during backoff");
                true
            }
        }
    }
}

/// Errors that can occur in the container daemon.
#[derive(Debug, Error)]
pub enum ContainerError {
    /// Failed to resolve the host address.
    #[error("could not resolve host address. Tried: {attempts}. Set --host-addr or DCBRIDGE_HOST")]
    HostResolution {
        /// Description of what was tried.
        attempts: String,
    },

    /// Failed to connect to the host control channel.
    #[error("failed to connect to host at {addr}: {source}")]
    Connect {
        /// The address we tried to connect to.
        addr: SocketAddr,
        /// The underlying error.
        source: ControlError,
    },

    /// Control channel error during operation.
    #[error("control channel error: {0}")]
    Control(#[from] ControlError),

    /// Port scanning error.
    #[error("scan error: {0}")]
    Scan(#[from] ScanError),

    /// Port filter configuration error.
    #[error("filter error: {0}")]
    Filter(#[from] filter::FilterError),

    /// Authentication failed — the host daemon rejected the auth token.
    ///
    /// This is a permanent error; retrying with the same token will not help.
    #[error("authentication failed: host daemon rejected the auth token")]
    AuthenticationFailed,

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve the host address using the resolution chain:
/// 1. CLI flag (`config.host_addr`)
/// 2. `DCBRIDGE_HOST` env var (already loaded into config)
/// 3. `host.docker.internal` DNS resolution
/// 4. Docker gateway IP from default route
/// 5. Fail with actionable error
///
/// # Errors
///
/// Returns [`ContainerError::HostResolution`] if no method succeeds.
pub async fn resolve_host_addr(config: &Config) -> Result<String, ContainerError> {
    let mut attempts = Vec::new();

    // 1 & 2: CLI flag / env var (both stored in config.host_addr)
    // If the value is a valid IP address, return it directly.
    // Otherwise treat it as a hostname and resolve via DNS so the
    // caller gets a numeric IP suitable for SocketAddr::parse.
    if let Some(ref addr) = config.host_addr {
        if addr.parse::<std::net::IpAddr>().is_ok() {
            return Ok(addr.clone());
        }
        // Attempt DNS resolution for hostnames like "host.docker.internal"
        match tokio::net::lookup_host(format!("{addr}:0")).await {
            Ok(mut addrs) => {
                if let Some(resolved) = addrs.next() {
                    let ip = resolved.ip().to_string();
                    info!(hostname = %addr, resolved = %ip, "resolved explicit host address via DNS");
                    return Ok(ip);
                }
            }
            Err(e) => {
                debug!(hostname = %addr, error = %e, "DNS lookup for explicit host address failed");
            }
        }
        // Fall back to returning the raw string — parse_addr will
        // produce a clear error if it's not a valid address.
        return Ok(addr.clone());
    }
    attempts.push("--host-addr flag / DCBRIDGE_HOST env");

    // 3: Try host.docker.internal DNS
    match tokio::net::lookup_host("host.docker.internal:0").await {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                let host = addr.ip().to_string();
                info!(host = %host, "resolved host via host.docker.internal");
                return Ok(host);
            }
        }
        Err(e) => {
            debug!(error = %e, "host.docker.internal DNS lookup failed");
        }
    }
    attempts.push("host.docker.internal DNS");

    // 4: Docker gateway IP from default route
    match resolve_gateway_ip().await {
        Some(ip) => {
            info!(host = %ip, "resolved host via default route gateway");
            return Ok(ip);
        }
        None => {
            debug!("could not determine gateway IP from default route");
        }
    }
    attempts.push("gateway IP from default route");

    Err(ContainerError::HostResolution {
        attempts: attempts.join(", "),
    })
}

/// Try to extract the gateway IP from the default route.
///
/// Runs `ip route` and parses `default via <IP>`.
async fn resolve_gateway_ip() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .arg("route")
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some("default"), Some("via"), Some(ip)) => Some(ip.to_owned()),
            _ => None,
        }
    })
}

/// Get the container ID, preferring the hostname.
///
/// In Docker, the hostname is typically set to the container short ID.
fn get_container_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Run the container daemon main loop.
///
/// Connects to the host daemon, registers, starts scanning for ports,
/// and handles incoming `ConnectRequest` messages.
///
/// # Arguments
///
/// * `config` — Runtime configuration (host address, ports, scan interval, etc.)
/// * `auth_token` — Authentication token to send in the Register message.
///   Empty string if no token is available (host may be running with `--no-auth`).
/// * `shutdown` — A future that resolves when the daemon should shut down.
///
/// # Errors
///
/// Returns [`ContainerError`] on fatal errors (host resolution failure,
/// unrecoverable control channel errors, authentication failure).
pub async fn run(
    config: Config,
    auth_token: String,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), ContainerError> {
    // Set up an internal shutdown channel that merges external shutdown
    // with Unix signals (SIGTERM, SIGHUP) and parent PID reparenting.
    let (internal_tx, internal_rx) = tokio::sync::watch::channel(false);

    // Forward the external shutdown signal
    let mut external_rx = shutdown.clone();
    let tx_external = internal_tx.clone();
    tokio::spawn(async move {
        let _ = external_rx.changed().await;
        let _ = tx_external.send(true);
    });

    // Handle SIGTERM and SIGHUP (Unix-only, which is fine — container daemon
    // always runs inside a Linux devcontainer)
    #[cfg(unix)]
    {
        let tx_term = internal_tx.clone();
        tokio::spawn(async move {
            let mut sigterm =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "failed to register SIGTERM handler");
                        return;
                    }
                };
            sigterm.recv().await;
            info!("received SIGTERM, shutting down");
            let _ = tx_term.send(true);
        });

        // Note: no SIGHUP handler — nohup sets SIGHUP to SIG_IGN and
        // registering a handler would override that, causing the daemon
        // to shut down when the launching shell (e.g. entrypoint)
        // exits. SIGTERM is sufficient for graceful shutdown.
    }

    // Drop the original tx so the channel closes when all senders are dropped
    drop(internal_tx);

    let mut shutdown = internal_rx;
    let host_addr_str = resolve_host_addr(&config).await?;
    let parse_addr = |port: u16| -> Result<SocketAddr, ContainerError> {
        format!("{host_addr_str}:{port}")
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                ContainerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            })
    };
    let control_addr = parse_addr(config.control_port)?;
    let data_addr = parse_addr(config.data_port)?;

    let scan_interval = Duration::from_millis(config.scan_interval_ms.max(100));
    let proc_path = Path::new("/proc");

    // Build the set of ports to exclude from scanning (self-exclusion of control/data ports)
    let mut exclude_ports: HashSet<u16> = config.exclude_ports.iter().copied().collect();
    exclude_ports.insert(config.control_port);
    exclude_ports.insert(config.data_port);

    // Build the port filter for include/exclude/process-regex filtering.
    // exclude_process and devcontainer.json path will be wired via CLI flags later;
    // for now, the filter handles exclude_ports and include_ports from config.
    let port_filter = PortFilter::new(
        &exclude_ports.iter().copied().collect::<Vec<_>>(),
        &config.include_ports,
        None,
        None,
    )?;

    let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(5));

    // Track ports across reconnections so we can re-Forward them
    let mut last_forwarded: HashMap<u16, ListeningPort> = HashMap::new();

    // Main reconnection loop
    loop {
        info!(%control_addr, "connecting to host daemon");

        let mut conn = match control::connect(control_addr).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, backoff_ms = backoff.current.as_millis(), "failed to connect to host, retrying");
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        };

        // Register with the host
        let container_id = get_container_id();

        info!(%container_id, "registering with host daemon");
        if let Err(e) = conn
            .send(&Message::Register {
                container_id: container_id.clone(),
                hostname: container_id.clone(),
                auth_token: auth_token.clone(),
            })
            .await
        {
            warn!(error = %e, "failed to send Register, reconnecting");
            if backoff.wait_or_shutdown(&mut shutdown).await {
                return Ok(());
            }
            continue;
        }

        // Wait for RegisterAck
        match conn.recv().await {
            Ok(Message::RegisterAck { success: true }) => {
                info!("registered successfully");
            }
            Ok(Message::RegisterAck { success: false }) => {
                error!(
                    "registration rejected by host (authentication failure). \
                     Check that the auth token matches the host daemon's token. \
                     Retrying will not help — exiting."
                );
                return Err(ContainerError::AuthenticationFailed);
            }
            Ok(other) => {
                warn!(?other, "unexpected message, expected RegisterAck");
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
            Err(e) => {
                warn!(error = %e, "failed to receive RegisterAck");
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        }

        // Connected successfully — reset backoff
        backoff.reset();

        // Re-Forward all previously tracked ports after reconnection
        if !last_forwarded.is_empty() {
            info!(
                count = last_forwarded.len(),
                "re-forwarding ports after reconnect"
            );
            let re_forward_failed = re_forward_ports(&mut conn, &last_forwarded).await;
            if re_forward_failed {
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        }

        let session_params = SessionParams {
            data_addr,
            proc_path,
            exclude_ports: &exclude_ports,
            port_filter: &port_filter,
            scan_interval,
        };

        // Run the connected session, passing in previously forwarded ports
        let (outcome, forwarded) =
            run_session(&mut conn, &session_params, &mut shutdown, last_forwarded).await;

        match outcome {
            SessionOutcome::Shutdown => {
                info!("shutdown signal received, exiting");
                return Ok(());
            }
            SessionOutcome::Disconnected => {
                last_forwarded = forwarded;
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
            }
            SessionOutcome::Error(e) => {
                warn!(error = %e, "session error, reconnecting");
                last_forwarded = forwarded;
                if backoff.wait_or_shutdown(&mut shutdown).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Re-forward all previously tracked ports. Returns `true` if any send failed.
///
/// ForwardAck responses are not consumed here — they arrive asynchronously
/// and are handled by `run_session`. A rejected re-forward is removed from
/// the forwarded map on the next ack, then re-detected on the next scan
/// cycle. This adds at most one scan interval of delay, which is acceptable
/// for the reconnection path.
async fn re_forward_ports(
    conn: &mut ControlConnection,
    forwarded: &HashMap<u16, ListeningPort>,
) -> bool {
    for (&port, lp) in forwarded {
        let msg = Message::Forward {
            port,
            protocol: Protocol::Tcp,
            process_name: lp.process_name.clone(),
            pid: lp.pid,
        };
        if let Err(e) = conn.send(&msg).await {
            warn!(port, error = %e, "failed to re-Forward port");
            return true;
        }
    }
    false
}

/// How a connected session ended.
///
/// Replaces `Result<(Exit, Map), (Error, Map)>` with a flat enum,
/// since the forwarded-ports map is always returned regardless.
enum SessionOutcome {
    /// A shutdown signal was received.
    Shutdown,
    /// The control connection was lost.
    Disconnected,
    /// An error occurred on the control channel.
    Error(ContainerError),
}

/// Parameters for a connected session, grouping values that don't change
/// between reconnections.
struct SessionParams<'a> {
    data_addr: SocketAddr,
    proc_path: &'a Path,
    exclude_ports: &'a HashSet<u16>,
    port_filter: &'a PortFilter,
    scan_interval: Duration,
}

/// Run a single connected session with the host daemon.
///
/// Scans for ports, sends Forward/Unforward diffs, and handles
/// incoming ConnectRequest messages. Returns the forwarded ports map
/// along with the exit reason so the caller can re-Forward them on reconnect.
async fn run_session(
    conn: &mut ControlConnection,
    params: &SessionParams<'_>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    initial_forwarded: HashMap<u16, ListeningPort>,
) -> (SessionOutcome, HashMap<u16, ListeningPort>) {
    // Track currently forwarded ports (initialized from previous session on reconnect)
    let mut forwarded = initial_forwarded;
    let mut scan_ticker = tokio::time::interval(params.scan_interval);

    // Channel for connect handlers to report failures back to the session
    // loop, which relays them as ConnectFailed on the control connection.
    let (fail_tx, mut fail_rx) = tokio::sync::mpsc::channel::<Message>(64);

    // Track spawned connect handler tasks for graceful drain on exit.
    let mut connect_handles: Vec<tokio::task::JoinHandle<Result<(), data::DataError>>> = Vec::new();

    let outcome = 'session: loop {
        tokio::select! {
            _ = scan_ticker.tick() => {
                // Scan for listening ports
                let current = match scanner::scan_listening_ports(params.proc_path, params.exclude_ports).await {
                    Ok(ports) => ports,
                    Err(e) => {
                        debug!(error = %e, "scan failed, skipping this cycle");
                        continue;
                    }
                };

                // Apply port filter (exclude, include, process regex)
                let filtered = params.port_filter.filter(current);
                let current_map: HashMap<u16, &ListeningPort> = filtered
                    .iter()
                    .map(|lp| (lp.port, lp))
                    .collect();

                // Detect new ports
                let mut send_err = None;
                for (port, lp) in &current_map {
                    if !forwarded.contains_key(port) {
                        info!(port, process = ?lp.process_name, "new listening port detected");
                        let msg = Message::Forward {
                            port: *port,
                            protocol: Protocol::Tcp,
                            process_name: lp.process_name.clone(),
                            pid: lp.pid,
                        };
                        if let Err(e) = conn.send(&msg).await {
                            warn!(port, error = %e, "failed to send Forward");
                            send_err = Some(e);
                            break;
                        }
                        forwarded.insert(*port, (*lp).clone());
                    }
                }
                if let Some(e) = send_err {
                    break 'session SessionOutcome::Error(ContainerError::Control(e));
                }

                // Detect removed ports
                let removed: Vec<u16> = forwarded
                    .keys()
                    .filter(|p| !current_map.contains_key(p))
                    .copied()
                    .collect();

                for port in removed {
                    info!(port, "listening port removed");
                    let msg = Message::Unforward { port };
                    if let Err(e) = conn.send(&msg).await {
                        warn!(port, error = %e, "failed to send Unforward");
                        send_err = Some(e);
                        break;
                    }
                    forwarded.remove(&port);
                }
                if let Some(e) = send_err {
                    break 'session SessionOutcome::Error(ContainerError::Control(e));
                }
            }

            msg_result = conn.recv() => {
                match msg_result {
                    Ok(Message::ConnectRequest { port, conn_id }) => {
                        info!(port, %conn_id, "received ConnectRequest");
                        let handle = spawn_connect_handler(port, conn_id, params.data_addr, fail_tx.clone());
                        connect_handles.push(handle);
                        // Prune completed handles to avoid unbounded growth
                        connect_handles.retain(|h| !h.is_finished());
                    }
                    Ok(Message::Ping) => {
                        debug!("received Ping, sending Pong");
                        if let Err(e) = conn.send(&Message::Pong).await {
                            warn!(error = %e, "failed to send Pong");
                            break 'session SessionOutcome::Error(ContainerError::Control(e));
                        }
                    }
                    Ok(Message::ForwardAck { port, success, host_port }) => {
                        if success {
                            info!(port, host_port, "forward acknowledged");
                        } else {
                            warn!(port, "forward request rejected by host");
                            forwarded.remove(&port);
                        }
                    }
                    Ok(other) => {
                        debug!(?other, "ignoring unexpected message");
                    }
                    Err(ControlError::ConnectionClosed) => {
                        warn!("control connection closed by host");
                        break 'session SessionOutcome::Disconnected;
                    }
                    Err(e) => {
                        warn!(error = %e, "control channel error");
                        break 'session SessionOutcome::Error(ContainerError::Control(e));
                    }
                }
            }

            // Relay ConnectFailed from connect handler tasks to the host
            Some(fail_msg) = fail_rx.recv() => {
                if let Err(e) = conn.send(&fail_msg).await {
                    warn!(error = %e, "failed to send ConnectFailed");
                    break 'session SessionOutcome::Error(ContainerError::Control(e));
                }
            }

            _ = shutdown.changed() => {
                // Send Unforward for all active ports before shutting down
                for &port in forwarded.keys() {
                    let msg = Message::Unforward { port };
                    if let Err(e) = conn.send(&msg).await {
                        debug!(port, error = %e, "failed to send Unforward during shutdown");
                    }
                }
                break 'session SessionOutcome::Shutdown;
            }
        }
    };

    // Drain in-flight connect handler tasks before returning, so we don't
    // abandon data connections mid-transfer.
    connect_handles.retain(|h| !h.is_finished());
    if !connect_handles.is_empty() {
        info!(
            count = connect_handles.len(),
            "draining in-flight connect handlers"
        );
        let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for handle in connect_handles {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = tokio::time::timeout(remaining, handle).await;
        }
    }

    (outcome, forwarded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn resolve_host_addr_returns_explicit_config() {
        let mut config = Config::default();
        config.host_addr = Some("10.0.0.1".to_string());
        let result = resolve_host_addr(&config).await.unwrap();
        assert_eq!(result, "10.0.0.1");
    }

    #[tokio::test]
    async fn resolve_host_addr_skips_fallbacks_with_explicit() {
        // Even with an unusual address, the explicit path returns it directly
        let mut config = Config::default();
        config.host_addr = Some("192.168.99.99".to_string());
        let result = resolve_host_addr(&config).await.unwrap();
        assert_eq!(result, "192.168.99.99");
    }

    #[test]
    fn get_container_id_returns_non_empty() {
        let id = get_container_id();
        assert!(!id.is_empty(), "container ID should never be empty");
    }
}
