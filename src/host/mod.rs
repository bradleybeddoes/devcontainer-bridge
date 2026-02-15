//! Host daemon — manages port forwarding and proxying for connected containers.
//!
//! The host daemon listens on two ports:
//! - **Control port** (default 19285): JSON-line protocol for container registration
//!   and forward/unforward commands.
//! - **Data port** (default 19286): Reverse data connections from containers for
//!   TCP proxying.

pub mod browser;
pub mod ensure;
pub mod listener;
pub mod proxy;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::control::{self, ControlConnection, ControlError, ControlListener};
use crate::protocol::{ForwardInfo, Message, Protocol};

use browser::BrowserOpener;
use listener::{start_listener, ClientConnection, ListenerError};
use proxy::{
    bridge_connection, new_pending_connections, register_pending, resolve_pending,
    PendingConnections,
};

/// Interval between heartbeat pings sent to containers.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Number of missed pongs before a container is considered dead.
const MAX_MISSED_PONGS: u32 = 3;

/// Default timeout for draining active proxy connections on forward teardown.
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of concurrent containers.
const MAX_CONTAINERS: usize = 64;

/// Maximum number of port forwards per container.
const MAX_FORWARDS_PER_CONTAINER: usize = 128;

/// Maximum length of a container_id or hostname string.
const MAX_IDENTIFIER_LENGTH: usize = 256;

/// Maximum length of a conn_id string.
const MAX_CONN_ID_LENGTH: usize = 128;

/// Monotonically increasing registration epoch counter.
///
/// Each new container registration gets a unique ID so that a stale
/// disconnect handler (from a superseded connection) does not remove
/// a newer registration's state.
static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

/// Check whether an identifier (container_id or hostname) is safe.
///
/// Rejects empty strings, strings exceeding [`MAX_IDENTIFIER_LENGTH`],
/// and strings containing characters outside the allowed set
/// (alphanumeric, hyphen, underscore, dot) to prevent log injection
/// via control characters or newlines.
fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_IDENTIFIER_LENGTH
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Errors that can occur when handling a Forward request.
#[derive(Debug, Error)]
enum ForwardError {
    /// Per-container forward limit exceeded.
    #[error("container has reached the {max}-forward limit")]
    TooManyForwards {
        /// The maximum number of forwards allowed per container.
        max: usize,
    },

    /// Failed to bind the per-port listener.
    #[error(transparent)]
    Listener(#[from] ListenerError),
}

/// Errors that can occur in the host daemon.
#[derive(Debug, Error)]
pub enum HostError {
    /// Failed to bind the control or data listener.
    #[error("failed to bind {role} on port {port}: {source}")]
    Bind {
        /// Which listener failed ("control" or "data").
        role: &'static str,
        /// The port we attempted to bind.
        port: u16,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to register a signal handler.
    #[error("failed to set up signal handler: {0}")]
    SignalSetup(std::io::Error),

    /// Control channel error.
    #[error("control channel error: {0}")]
    Control(#[from] ControlError),

    /// Listener error.
    #[error("listener error: {0}")]
    Listener(#[from] ListenerError),
}

/// Tracks active proxy connection count with drain notification.
///
/// Each forwarded port has a `ConnectionTracker` shared between the
/// listener accept loop and the proxy bridge tasks. When a forward is
/// being torn down, the caller awaits [`wait_drained`] which unblocks
/// once all in-flight proxy connections have finished.
///
/// Uses `Notify::notify_one` which stores a permit when no waiter
/// is present, so `wait_drained` never misses a zero-crossing.
struct ConnectionTracker {
    /// Number of active proxy connections through this forward.
    count: AtomicUsize,
    /// Notified when `count` drops to zero.
    drained: Notify,
    /// Signals proxy tasks to drop their connections on drain timeout.
    cancel_tx: watch::Sender<bool>,
}

impl ConnectionTracker {
    /// Create a tracker with zero active connections.
    fn new() -> Self {
        let (cancel_tx, _) = watch::channel(false);
        Self {
            count: AtomicUsize::new(0),
            drained: Notify::new(),
            cancel_tx,
        }
    }

    /// Subscribe to the cancellation signal for use in proxy tasks.
    fn cancel_rx(&self) -> watch::Receiver<bool> {
        self.cancel_tx.subscribe()
    }

    /// Signal all subscribed proxy tasks to drop their connections.
    fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Record a new proxy connection starting.
    fn increment(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
    }

    /// Record a proxy connection finishing; notifies drain waiters on zero.
    fn decrement(&self) {
        if self.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_one();
        }
    }

    /// Return the current number of active connections.
    fn active(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Wait until all active connections have finished.
    ///
    /// Registers the `Notify` future *before* checking the count to avoid
    /// missing a `notify_one` that fires between the check and the await.
    async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            if self.active() == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Per-port forwarding state.
///
/// Created when a container sends `Forward` and removed on `Unforward`
/// or container disconnect. Owns the listener shutdown channel and
/// tracks active proxy connections for graceful draining.
struct ForwardState {
    /// The port bound on the host.
    host_port: u16,
    /// Shutdown sender for the listener task.
    shutdown_tx: watch::Sender<bool>,
    /// Join handle for the listener task.
    handle: JoinHandle<()>,
    /// Optional process name.
    process_name: Option<String>,
    /// Optional PID.
    pid: Option<u32>,
    /// Unix epoch seconds when the forward was established.
    since: String,
    /// Active proxy connection tracker for graceful draining.
    tracker: Arc<ConnectionTracker>,
}

/// A request queued from a client connection handler to be sent to a
/// container via its control channel.
///
/// Routed through an `mpsc` channel so that the container message loop
/// (which owns the `ControlConnection` write half) serializes outbound
/// `ConnectRequest` messages without concurrent write conflicts.
struct OutboundConnectRequest {
    /// The container port the client wants to reach.
    port: u16,
    /// Unique connection identifier.
    conn_id: String,
}

/// Per-container state stored while a container is registered.
///
/// Holds the container's active forwards and the channel used to
/// route `ConnectRequest` messages to its control connection handler.
struct ContainerState {
    /// Registration epoch — prevents a stale disconnect handler from
    /// cleaning up a newer registration's state.
    registration_id: u64,
    /// Human-readable hostname.
    hostname: String,
    /// Active forwards: container_port → ForwardState.
    forwards: HashMap<u16, ForwardState>,
    /// Channel to send ConnectRequests to this container's control connection.
    connect_request_tx: mpsc::Sender<OutboundConnectRequest>,
}

/// Top-level mutable state for the host daemon, protected by a `Mutex`.
///
/// Contains all connected containers and the set of host ports in use.
/// Port allocation, container lookup, and forward-info collection all
/// operate on this struct.
struct HostState {
    /// Connected containers: container_id → ContainerState.
    containers: HashMap<String, ContainerState>,
    /// Set of host ports currently in use (for conflict resolution).
    used_host_ports: HashMap<u16, String>,
}

impl HostState {
    fn new() -> Self {
        Self {
            containers: HashMap::new(),
            used_host_ports: HashMap::new(),
        }
    }

    /// Find the next available host port starting from `preferred`.
    ///
    /// Returns `preferred` if it is free, otherwise scans upward
    /// wrapping around the port space. Returns `0` if every port
    /// is in use (the subsequent bind will fail cleanly).
    fn find_available_port(&self, preferred: u16) -> u16 {
        let mut port = preferred;
        while self.used_host_ports.contains_key(&port) {
            port = port.wrapping_add(1);
            if port == 0 {
                port = 1024;
            }
            if port == preferred {
                // Wrapped all the way around — no ports available
                return 0;
            }
        }
        port
    }

    /// Find which container owns a given container port forward.
    fn find_container_for_port(&self, container_port: u16) -> Option<&str> {
        self.containers
            .iter()
            .find(|(_, state)| state.forwards.contains_key(&container_port))
            .map(|(cid, _)| cid.as_str())
    }

    /// Build ForwardInfo list for ListResponse.
    fn collect_forward_info(&self) -> Vec<ForwardInfo> {
        let mut infos = Vec::new();
        for (cid, cstate) in &self.containers {
            for (port, fstate) in &cstate.forwards {
                infos.push(ForwardInfo {
                    container_id: cid.clone(),
                    hostname: cstate.hostname.clone(),
                    port: *port,
                    host_port: fstate.host_port,
                    protocol: Protocol::Tcp,
                    process_name: fstate.process_name.clone(),
                    pid: fstate.pid,
                    since: fstate.since.clone(),
                });
            }
        }
        infos
    }
}

/// Shared resources threaded through control connection handlers.
///
/// Groups the `Arc`-wrapped state, pending-connections map, browser
/// opener, and client-connection channel so they can be passed as a
/// single reference instead of 4+ separate parameters.
struct DaemonContext {
    /// Protected host daemon state (containers, port allocations).
    state: Mutex<HostState>,
    /// Pending reverse data connections awaiting `ConnectReady`.
    pending: PendingConnections,
    /// Browser opener with port rewriting and rate limiting.
    browser: Arc<Mutex<BrowserOpener>>,
    /// Channel that port listeners use to deliver client connections.
    client_tx: mpsc::Sender<ClientConnection>,
}

/// Configuration for the host daemon.
pub struct HostConfig {
    /// Control channel port (default: 19285).
    pub control_port: u16,
    /// Data channel port (default: 19286).
    pub data_port: u16,
    /// IP address to bind the control and data listeners to.
    ///
    /// When `None`, the bind address is resolved automatically via
    /// [`resolve_bind_addr`]: if Docker is detected, binds to `0.0.0.0`;
    /// otherwise binds to `127.0.0.1`.
    ///
    /// Forwarded per-port listeners always bind to loopback regardless of this
    /// setting.
    pub bind_addr: Option<std::net::IpAddr>,
    /// Skip Docker auto-detection and default to `127.0.0.1`.
    ///
    /// Only relevant when `bind_addr` is `None`. When `true`, Docker detection
    /// is skipped and the bind address defaults to loopback.
    pub no_docker_detect: bool,
    /// Exit when the last container disconnects.
    pub exit_on_idle: bool,
    /// Timeout for draining active connections on forward teardown (default: 5s).
    pub drain_timeout: Duration,
    /// Custom browser command to use instead of `open` (macOS) / `xdg-open` (Linux).
    ///
    /// Useful for testing (e.g. `/usr/bin/true`) or headless environments.
    pub browser_cmd: Option<String>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            control_port: 19285,
            data_port: 19286,
            bind_addr: None,
            no_docker_detect: false,
            exit_on_idle: true,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            browser_cmd: None,
        }
    }
}

/// Detect whether Docker is running by executing `docker info`.
///
/// Returns `true` if the `docker info` command exits successfully, indicating
/// Docker (Desktop or Engine) is available and running.
async fn detect_docker() -> bool {
    tokio::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve the effective bind address for the control and data listeners.
///
/// Resolution priority:
/// 1. `config.bind_addr` explicitly set -> use that address
/// 2. `config.no_docker_detect` set -> bind to `127.0.0.1`
/// 3. Docker detected via `docker info` -> bind to `0.0.0.0`
/// 4. No Docker detected -> bind to `127.0.0.1`
async fn resolve_bind_addr(config: &HostConfig) -> std::net::IpAddr {
    if let Some(addr) = config.bind_addr {
        info!("Using configured bind address {}", addr);
        return addr;
    }

    if config.no_docker_detect {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        info!(
            "No Docker detected, binding to {}. Use --bind-addr to override.",
            addr
        );
        return addr;
    }

    if detect_docker().await {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        info!(
            "Docker detected, binding control/data ports to {} for container connectivity. \
             Use --bind-addr 127.0.0.1 or --no-docker-detect to restrict.",
            addr
        );
        addr
    } else {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        info!(
            "No Docker detected, binding to {}. Use --bind-addr to override.",
            addr
        );
        addr
    }
}

/// Generate a simple Unix-epoch timestamp string for the `since` field.
///
/// Returns seconds since the Unix epoch as a string. A proper ISO 8601
/// timestamp requires the `chrono` crate; this is sufficient for MVP.
fn timestamp_now() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

/// Run the host daemon.
///
/// This is the main entry point. It binds control and data listeners and
/// processes container connections until shutdown.
///
/// # Errors
///
/// Returns [`HostError`] if the daemon cannot start (e.g. port bind failure).
pub async fn run(config: HostConfig) -> Result<(), HostError> {
    let bind_addr = resolve_bind_addr(&config).await;

    let control_listener = ControlListener::bind(bind_addr, config.control_port)
        .await
        .map_err(|e| match e {
            ControlError::Io(source) => HostError::Bind {
                role: "control",
                port: config.control_port,
                source,
            },
            other => HostError::Control(other),
        })?;
    info!(
        port = config.control_port,
        bind_addr = %bind_addr,
        "control listener bound"
    );

    let data_addr: SocketAddr = (bind_addr, config.data_port).into();
    let data_listener = TcpListener::bind(data_addr)
        .await
        .map_err(|source| HostError::Bind {
            role: "data",
            port: config.data_port,
            source,
        })?;
    info!(
        port = config.data_port,
        bind_addr = %bind_addr,
        "data listener bound"
    );

    // Channel for client connections from all port listeners
    let (client_tx, mut client_rx) = mpsc::channel::<ClientConnection>(256);

    let ctx = Arc::new(DaemonContext {
        state: Mutex::new(HostState::new()),
        pending: new_pending_connections(),
        browser: Arc::new(Mutex::new(BrowserOpener::with_cmd(
            config.browser_cmd.clone(),
        ))),
        client_tx,
    });
    let drain_timeout = config.drain_timeout;

    // Channel to signal daemon shutdown
    let (daemon_shutdown_tx, mut daemon_shutdown_rx) = mpsc::channel::<()>(1);

    // Set up SIGTERM handler (Unix only; the host daemon targets macOS/Linux)
    #[cfg(unix)]
    let mut sigterm = Some(
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(HostError::SignalSetup)?,
    );
    #[cfg(not(unix))]
    let mut sigterm: Option<tokio::signal::unix::Signal> = None;

    loop {
        // Create a future that resolves on SIGTERM (Unix) or never resolves.
        let sigterm_fut = async {
            match sigterm.as_mut() {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            // Accept new control connections (containers or CLI clients)
            result = control_listener.accept() => {
                match result {
                    Ok((conn, addr)) => {
                        info!(%addr, "accepted control connection");
                        let ctx = Arc::clone(&ctx);
                        let shutdown_signal = daemon_shutdown_tx.clone();
                        let exit_on_idle = config.exit_on_idle;
                        tokio::spawn(async move {
                            if let Err(e) = handle_control_connection(
                                conn, addr, &ctx, shutdown_signal, exit_on_idle,
                            ).await {
                                warn!(%addr, error = %e, "control connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "failed to accept control connection");
                    }
                }
            }

            // Accept data connections (reverse data from containers)
            result = data_listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        debug!(%addr, "accepted data connection");
                        let pending = Arc::clone(&ctx.pending);
                        tokio::spawn(async move {
                            if let Err(e) = handle_data_connection(stream, addr, pending).await {
                                warn!(%addr, error = %e, "data connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "failed to accept data connection");
                    }
                }
            }

            // Handle client connections from port listeners
            Some(client_conn) = client_rx.recv() => {
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move {
                    handle_client_connection(client_conn, &ctx).await;
                });
            }

            // Daemon shutdown signal (internal)
            _ = daemon_shutdown_rx.recv() => {
                info!("received shutdown signal, stopping host daemon");
                break;
            }

            // SIGINT (Ctrl+C)
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, stopping host daemon");
                break;
            }

            // SIGTERM
            _ = sigterm_fut => {
                info!("received SIGTERM, stopping host daemon");
                break;
            }
        }
    }

    // Tear down all active forwards with graceful drain
    let mut state = ctx.state.lock().await;
    drain_all_forwards(&mut state, drain_timeout).await;

    // Clean up the PID file written by `dbr ensure`
    if let Err(e) = ensure::remove_pid_file() {
        debug!(error = %e, "could not remove PID file (non-fatal)");
    }

    info!("host daemon stopped");
    Ok(())
}

/// Handle a single control connection (from a container or CLI).
async fn handle_control_connection(
    mut conn: ControlConnection,
    addr: SocketAddr,
    ctx: &DaemonContext,
    shutdown_signal: mpsc::Sender<()>,
    exit_on_idle: bool,
) -> Result<(), ControlError> {
    // First message should be Register or ListRequest
    let first_msg = conn.recv().await?;

    match first_msg {
        Message::Register {
            container_id,
            hostname,
        } => {
            // Validate identifier content and length
            if !is_valid_identifier(&container_id) || !is_valid_identifier(&hostname) {
                warn!(
                    %addr,
                    container_id_len = container_id.len(),
                    hostname_len = hostname.len(),
                    "rejecting registration: invalid identifier"
                );
                conn.send(&Message::RegisterAck { success: false }).await?;
                return Ok(());
            }

            // Enforce container limit
            {
                let s = ctx.state.lock().await;
                if s.containers.len() >= MAX_CONTAINERS && !s.containers.contains_key(&container_id)
                {
                    warn!(
                        %addr, container_id,
                        max = MAX_CONTAINERS,
                        "rejecting registration: too many containers"
                    );
                    conn.send(&Message::RegisterAck { success: false }).await?;
                    return Ok(());
                }
            }

            // Clean up stale state if this container_id is already registered
            // (e.g. reconnect after network disruption before heartbeat timeout)
            {
                let s = ctx.state.lock().await;
                if s.containers.contains_key(&container_id) {
                    warn!(%addr, container_id, "re-registration, cleaning up old state");
                    drop(s);
                    cleanup_container(&container_id, &ctx.state).await;
                }
            }

            info!(
                %addr, container_id, hostname,
                "container registered"
            );

            // Acknowledge registration
            conn.send(&Message::RegisterAck { success: true }).await?;

            // Create channel for outbound ConnectRequests to this container
            let (connect_req_tx, connect_req_rx) = mpsc::channel::<OutboundConnectRequest>(64);

            // Register in state with a unique epoch ID
            let reg_id = NEXT_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed);
            {
                let mut s = ctx.state.lock().await;
                s.containers.insert(
                    container_id.clone(),
                    ContainerState {
                        registration_id: reg_id,
                        hostname,
                        forwards: HashMap::new(),
                        connect_request_tx: connect_req_tx,
                    },
                );
            }

            // Process messages from this container until disconnect
            let result =
                handle_container_messages(&mut conn, &container_id, ctx, connect_req_rx).await;

            // Clean up on disconnect — only if this registration still owns the
            // state. A re-registration replaces the ContainerState with a new
            // registration_id, so a stale disconnect must not remove it.
            let should_cleanup = {
                let s = ctx.state.lock().await;
                s.containers
                    .get(&container_id)
                    .is_some_and(|c| c.registration_id == reg_id)
            };
            if should_cleanup {
                cleanup_container(&container_id, &ctx.state).await;
                info!(container_id, "container disconnected");
            } else {
                info!(
                    container_id,
                    "stale connection closed (superseded by re-registration)"
                );
            }

            // Check if we should exit
            if exit_on_idle {
                let s = ctx.state.lock().await;
                if s.containers.is_empty() {
                    info!("last container disconnected, shutting down");
                    let _ = shutdown_signal.send(()).await;
                }
            }

            match result {
                Ok(()) | Err(ControlError::ConnectionClosed) => Ok(()),
                Err(e) => Err(e),
            }
        }
        Message::ListRequest => {
            let infos = ctx.state.lock().await.collect_forward_info();
            conn.send(&Message::ListResponse { forwards: infos })
                .await?;
            Ok(())
        }
        Message::Ping => {
            conn.send(&Message::Pong).await?;
            Ok(())
        }
        Message::OpenUrl { url } => {
            // One-shot OpenUrl from `dbr open` (no registration needed)
            let success = ctx
                .browser
                .lock()
                .await
                .open(&url)
                .await
                .inspect_err(|e| {
                    warn!(%addr, url, error = %e, "failed to open URL");
                })
                .is_ok();
            conn.send(&Message::OpenUrlAck { success }).await?;
            Ok(())
        }
        other => {
            warn!(%addr, message = ?other, "unexpected first message, expected Register or ListRequest");
            Ok(())
        }
    }
}

/// Process ongoing messages from a registered container.
///
/// Sends periodic heartbeat pings and disconnects if the container
/// fails to respond within [`MAX_MISSED_PONGS`] intervals.
/// Also forwards ConnectRequests from client connection handlers to the
/// container via its control connection.
async fn handle_container_messages(
    conn: &mut ControlConnection,
    container_id: &str,
    ctx: &DaemonContext,
    mut connect_req_rx: mpsc::Receiver<OutboundConnectRequest>,
) -> Result<(), ControlError> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick
    let mut missed_pongs: u32 = 0;

    loop {
        tokio::select! {
            result = conn.recv() => {
                let msg = result?;
                let was_pong = dispatch_container_message(
                    msg, conn, container_id, ctx,
                ).await?;
                if was_pong {
                    missed_pongs = 0;
                }
            }

            // Forward ConnectRequests from client connection handlers
            Some(req) = connect_req_rx.recv() => {
                debug!(container_id, conn_id = req.conn_id, port = req.port, "sending ConnectRequest to container");
                if let Err(e) = conn.send(&Message::ConnectRequest {
                    port: req.port,
                    conn_id: req.conn_id.clone(),
                }).await {
                    warn!(container_id, conn_id = req.conn_id, error = %e, "failed to send ConnectRequest");
                    proxy::cancel_pending(&ctx.pending, &req.conn_id).await;
                }
            }

            _ = heartbeat.tick() => {
                missed_pongs += 1;
                if missed_pongs > MAX_MISSED_PONGS {
                    warn!(
                        container_id,
                        missed_pongs,
                        "heartbeat timeout, disconnecting container"
                    );
                    return Err(ControlError::ConnectionClosed);
                }
                debug!(container_id, missed_pongs, "sending heartbeat ping");
                if let Err(e) = conn.send(&Message::Ping).await {
                    warn!(container_id, error = %e, "failed to send heartbeat ping");
                    return Err(e);
                }
            }
        }
    }
}

/// Dispatch a single message from a registered container.
///
/// Returns `Ok(true)` if the message was a `Pong` (caller resets heartbeat),
/// `Ok(false)` for all other handled messages.
async fn dispatch_container_message(
    msg: Message,
    conn: &mut ControlConnection,
    container_id: &str,
    ctx: &DaemonContext,
) -> Result<bool, ControlError> {
    match msg {
        Message::Forward {
            port,
            protocol: _,
            process_name,
            pid,
        } => {
            let result = handle_forward(
                container_id,
                port,
                process_name,
                pid,
                &ctx.state,
                &ctx.client_tx,
            )
            .await;
            match result {
                Ok(host_port) => {
                    if host_port != port {
                        info!(
                            container_id,
                            container_port = port,
                            host_port,
                            "port conflict resolved, assigned alternative host port"
                        );
                    } else {
                        info!(
                            container_id,
                            container_port = port,
                            host_port,
                            "port forwarded"
                        );
                    }
                    ctx.browser.lock().await.add_port_mapping(port, host_port);
                    conn.send(&Message::ForwardAck {
                        port,
                        success: true,
                        host_port,
                    })
                    .await?;
                }
                Err(e) => {
                    warn!(container_id, port, error = %e, "failed to forward port");
                    conn.send(&Message::ForwardAck {
                        port,
                        success: false,
                        host_port: 0,
                    })
                    .await?;
                }
            }
            Ok(false)
        }

        Message::Unforward { port } => {
            handle_unforward(container_id, port, &ctx.state).await;
            ctx.browser.lock().await.remove_port_mapping(port);
            info!(container_id, port, "port unforwarded");
            Ok(false)
        }

        Message::ConnectFailed { conn_id, error } => {
            if conn_id.len() > MAX_CONN_ID_LENGTH {
                warn!(
                    container_id,
                    "ignoring ConnectFailed with oversized conn_id"
                );
            } else {
                warn!(container_id, conn_id, error, "container connect failed");
                proxy::cancel_pending(&ctx.pending, &conn_id).await;
            }
            Ok(false)
        }

        Message::OpenUrl { url } => {
            let success = ctx
                .browser
                .lock()
                .await
                .open(&url)
                .await
                .inspect_err(|e| {
                    warn!(container_id, url, error = %e, "failed to open URL");
                })
                .is_ok();
            conn.send(&Message::OpenUrlAck { success }).await?;
            Ok(false)
        }

        Message::Ping => {
            conn.send(&Message::Pong).await?;
            Ok(false)
        }

        Message::Pong => {
            debug!(container_id, "received pong, resetting heartbeat counter");
            Ok(true)
        }

        other => {
            debug!(container_id, message = ?other, "ignoring unexpected message");
            Ok(false)
        }
    }
}

/// Handle a Forward request: bind a listener and track the forward.
///
/// The port availability check and listener bind are not atomic — another
/// concurrent Forward could race to the same port. This is benign: the
/// loser's `start_listener` bind fails with a `ListenerError`, which is
/// propagated as a `ForwardAck { success: false }` to the container.
async fn handle_forward(
    container_id: &str,
    port: u16,
    process_name: Option<String>,
    pid: Option<u32>,
    state: &Mutex<HostState>,
    client_tx: &mpsc::Sender<ClientConnection>,
) -> Result<u16, ForwardError> {
    // Enforce per-container forward limit
    {
        let s = state.lock().await;
        if let Some(cstate) = s.containers.get(container_id) {
            if cstate.forwards.len() >= MAX_FORWARDS_PER_CONTAINER {
                warn!(
                    container_id,
                    port,
                    max = MAX_FORWARDS_PER_CONTAINER,
                    "rejecting forward: too many forwards for this container"
                );
                return Err(ForwardError::TooManyForwards {
                    max: MAX_FORWARDS_PER_CONTAINER,
                });
            }
        }
    }

    let target_port = {
        let s = state.lock().await;
        s.find_available_port(port)
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (host_port, handle) = start_listener(target_port, shutdown_rx, client_tx.clone()).await?;

    let now = timestamp_now();
    let tracker = Arc::new(ConnectionTracker::new());

    let mut s = state.lock().await;
    if let Some(cstate) = s.containers.get_mut(container_id) {
        cstate.forwards.insert(
            port,
            ForwardState {
                host_port,
                shutdown_tx,
                handle,
                process_name,
                pid,
                since: now,
                tracker,
            },
        );
    }
    s.used_host_ports
        .insert(host_port, container_id.to_string());

    Ok(host_port)
}

/// Handle an Unforward request: stop the listener and drain active connections.
async fn handle_unforward(container_id: &str, port: u16, state: &Mutex<HostState>) {
    let forward = {
        let mut s = state.lock().await;
        s.containers
            .get_mut(container_id)
            .and_then(|cstate| cstate.forwards.remove(&port))
            .inspect(|fstate| {
                s.used_host_ports.remove(&fstate.host_port);
            })
    };

    if let Some(fstate) = forward {
        let _ = fstate.shutdown_tx.send(true);
        let _ = fstate.handle.await;
        drain_forward_connections(&fstate.tracker, port, DEFAULT_DRAIN_TIMEOUT).await;
    }
}

/// Clean up all state for a disconnected container.
async fn cleanup_container(container_id: &str, state: &Mutex<HostState>) {
    let forwards = {
        let mut s = state.lock().await;
        let Some(cstate) = s.containers.remove(container_id) else {
            return;
        };
        cstate
            .forwards
            .into_iter()
            .map(|(port, fstate)| {
                info!(
                    container_id,
                    port, "tearing down forward on container disconnect"
                );
                s.used_host_ports.remove(&fstate.host_port);
                let _ = fstate.shutdown_tx.send(true);
                (port, fstate.handle, fstate.tracker)
            })
            .collect::<Vec<_>>()
    };

    // Await all listener handles first to ensure ports are freed,
    // then drain active proxy connections concurrently.
    let mut drain_set = tokio::task::JoinSet::new();
    for (port, handle, tracker) in forwards {
        let _ = handle.await;
        drain_set.spawn(async move {
            drain_forward_connections(&tracker, port, DEFAULT_DRAIN_TIMEOUT).await;
        });
    }
    while drain_set.join_next().await.is_some() {}
}

/// Drain all forwards across all containers during daemon shutdown.
async fn drain_all_forwards(state: &mut HostState, drain_timeout: Duration) {
    let mut handles = Vec::new();

    for (_cid, cstate) in state.containers.drain() {
        for (port, fstate) in cstate.forwards {
            let _ = fstate.shutdown_tx.send(true);
            handles.push((port, fstate.handle, fstate.tracker));
        }
    }
    state.used_host_ports.clear();

    let mut drain_set = tokio::task::JoinSet::new();
    for (port, handle, tracker) in handles {
        let _ = handle.await;
        drain_set.spawn(async move {
            drain_forward_connections(&tracker, port, drain_timeout).await;
        });
    }
    while drain_set.join_next().await.is_some() {}
}

/// Wait for active proxy connections to drain, up to the given timeout.
async fn drain_forward_connections(tracker: &ConnectionTracker, port: u16, timeout: Duration) {
    let count = tracker.active();
    if count == 0 {
        return;
    }

    info!(
        port,
        active_connections = count,
        "draining active connections"
    );

    match tokio::time::timeout(timeout, tracker.wait_drained()).await {
        Ok(()) => {
            info!(port, "all connections drained");
        }
        Err(_) => {
            let remaining = tracker.active();
            warn!(
                port,
                remaining_connections = remaining,
                timeout_secs = timeout.as_secs(),
                "drain timeout expired, force-closing remaining connections"
            );
            tracker.cancel();
        }
    }
}

/// Handle a data connection: read the ConnectReady handshake and dispatch.
async fn handle_data_connection(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    pending: PendingConnections,
) -> Result<(), ControlError> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // SECURITY: Use bounded read_message instead of unbounded read_line to
    // prevent OOM from a malicious peer sending data without a newline.
    let msg = control::read_message(&mut reader).await?;

    match msg {
        Message::ConnectReady { conn_id } => {
            if conn_id.len() > MAX_CONN_ID_LENGTH {
                warn!(%addr, "rejecting data connection with oversized conn_id");
                return Ok(());
            }
            // Extract any data the BufReader has already buffered beyond the
            // ConnectReady line (e.g. payload from the container's local
            // service that arrived in the same TCP segment).
            let buffered = reader.buffer().to_vec();
            if !buffered.is_empty() {
                debug!(%addr, conn_id, buffered_bytes = buffered.len(), "data connection has pre-read bytes");
            } else {
                debug!(%addr, conn_id, "data connection ready");
            }
            // Reunite the stream for raw TCP proxying
            let stream = reader
                .into_inner()
                .reunite(write_half)
                .map_err(|e| ControlError::Io(std::io::Error::other(e.to_string())))?;
            resolve_pending(&pending, &conn_id, proxy::DataStream { stream, buffered }).await;
            Ok(())
        }
        other => {
            warn!(%addr, message = ?other, "unexpected message on data connection");
            Ok(())
        }
    }
}

/// Handle a client connection to a forwarded port.
///
/// Sends a ConnectRequest to the container and waits for the reverse
/// data connection to be established, then bridges bidirectionally.
/// Tracks active connection count for graceful draining on teardown.
async fn handle_client_connection(client_conn: ClientConnection, ctx: &DaemonContext) {
    let port = client_conn.container_port;
    let peer = client_conn.peer_addr;

    // Find which container owns this port, get connection tracking and connect request channel
    let (container_id, tracker, connect_tx) = {
        let s = ctx.state.lock().await;
        let cid = s.find_container_for_port(port).map(|s| s.to_string());
        let trk = cid.as_ref().and_then(|id| {
            s.containers
                .get(id)
                .and_then(|c| c.forwards.get(&port))
                .map(|f| Arc::clone(&f.tracker))
        });
        let tx = cid
            .as_ref()
            .and_then(|id| s.containers.get(id))
            .map(|c| c.connect_request_tx.clone());
        (cid, trk, tx)
    };

    let container_id = match container_id {
        Some(id) => id,
        None => {
            warn!(port, %peer, "no container owns this forwarded port");
            return;
        }
    };

    let connect_tx = match connect_tx {
        Some(tx) => tx,
        None => {
            warn!(port, %peer, container_id, "no connect request channel for container");
            return;
        }
    };

    // Track this connection for drain support
    if let Some(ref trk) = tracker {
        trk.increment();
    }

    let conn_id = uuid::Uuid::new_v4().to_string();
    debug!(conn_id, container_id, port, %peer, "initiating proxy for client connection");

    // Register pending connection, then send ConnectRequest to the container
    // via its control channel. The container will open a reverse data connection
    // back to the host data port with a ConnectReady handshake.
    let data_rx = register_pending(&ctx.pending, conn_id.clone()).await;

    // Send ConnectRequest to the container's control message loop
    if let Err(e) = connect_tx
        .send(OutboundConnectRequest {
            port,
            conn_id: conn_id.clone(),
        })
        .await
    {
        warn!(conn_id, port, error = %e, "failed to queue ConnectRequest");
        proxy::cancel_pending(&ctx.pending, &conn_id).await;
        if let Some(trk) = tracker {
            trk.decrement();
        }
        return;
    }

    // Bridge when the data connection arrives (or timeout).
    // If the forward is being torn down, the cancel signal drops the
    // bridge future, force-closing both TCP streams.
    let bridge_result = match tracker.as_ref() {
        Some(trk) => {
            let mut cancel = trk.cancel_rx();
            tokio::select! {
                result = bridge_connection(conn_id.clone(), client_conn.stream, data_rx, &ctx.pending) => Some(result),
                _ = cancel.changed() => {
                    debug!(conn_id, port, "proxy force-closed by drain");
                    None
                }
            }
        }
        None => Some(
            bridge_connection(conn_id.clone(), client_conn.stream, data_rx, &ctx.pending).await,
        ),
    };

    match bridge_result {
        Some(Ok((c2s, s2c))) => {
            info!(
                conn_id,
                port,
                bytes_client_to_container = c2s,
                bytes_container_to_client = s2c,
                "proxy completed"
            );
        }
        Some(Err(e)) => {
            debug!(conn_id, port, error = %e, "proxy failed");
        }
        None => {} // force-closed by drain, already logged
    }

    // Decrement active connection count
    if let Some(trk) = tracker {
        trk.decrement();
    }
}
