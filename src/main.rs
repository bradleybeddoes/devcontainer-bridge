//! CLI entrypoint for the `dbr` (devcontainer-bridge) binary.
//!
//! Parses command-line arguments, sets up tracing, and dispatches to the
//! appropriate module.
//!
//! When invoked as `dbr-open` (via hardlink), the binary treats its first
//! positional argument as a URL and behaves like `dbr open <URL>`.

mod cli;

use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;

use clap::Parser;
use tracing::{debug, error, info};

use thiserror::Error;

use dbr::config::{Config, SocketForwardingConfig};
use dbr::container::browser;
use dbr::control::{self, ControlConnection, ControlError};
use dbr::host::HostConfig;
use dbr::protocol::{ForwardInfo, Message, Protocol};

use cli::{Cli, Command};

/// Errors from CLI subcommands that interact with the host daemon.
#[derive(Debug, Error)]
enum CliError {
    /// Could not connect to or communicate with the host daemon.
    #[error("{0}")]
    Connection(String),

    /// Control channel protocol error.
    #[error(transparent)]
    Control(#[from] ControlError),

    /// The host daemon rejected the CLI registration.
    #[error("host daemon rejected registration")]
    RegistrationRejected,

    /// JSON serialization error (status --json output).
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Create a tokio runtime, printing to stderr and returning `FAILURE` on error.
fn build_runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Runtime::new().map_err(|e| {
        eprintln!("failed to create tokio runtime: {e}");
        ExitCode::FAILURE
    })
}

/// Run an async block on a new runtime, mapping errors to `ExitCode::FAILURE`.
fn run_async<F, E>(f: F) -> ExitCode
where
    F: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let rt = match build_runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    if let Err(e) = rt.block_on(f) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    // When invoked as `dbr-open` (hardlink), behave as `dbr open <URL>`.
    if invoked_as_dbr_open() {
        return run_dbr_open();
    }

    let cli = Cli::parse();

    match cli.command {
        Command::HostDaemon {
            bind_addr,
            no_docker_detect,
            control_port,
            data_port,
            log_level,
            log_format,
            log_file,
            exit_on_idle,
            browser_cmd,
            auth_token,
            auth_token_file,
            no_auth,
            socket_watch_paths,
            socket_container_path_prefix,
            socket_scan_interval_ms,
            no_socket_forwarding,
        } => {
            init_tracing(&log_level, &log_format, log_file.as_deref());

            let resolved_auth_token = if no_auth {
                eprintln!(
                    "WARNING: running without authentication. Any process that can \
                     reach the control port can request forwards."
                );
                None
            } else {
                let token_file = auth_token_file.as_ref().map(std::path::Path::new);
                let default_path = match dbr::auth::token_file_path() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("auth error: {e}");
                        return ExitCode::FAILURE;
                    }
                };

                match dbr::auth::resolve_token(auth_token.as_deref(), token_file, &default_path) {
                    Ok(token) => Some(token),
                    Err(dbr::auth::AuthError::NoTokenSource { .. }) => {
                        // No existing token — generate one
                        match dbr::auth::ensure_token(&default_path) {
                            Ok(token) => Some(token),
                            Err(e) => {
                                eprintln!("auth error: {e}");
                                return ExitCode::FAILURE;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("auth error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            };

            // Build socket forwarding config: CLI flags override config file
            let loaded_config = Config::from_env().unwrap_or_default();
            let socket_forwarding = if no_socket_forwarding {
                SocketForwardingConfig {
                    enabled: false,
                    ..SocketForwardingConfig::default()
                }
            } else {
                let mut sf = loaded_config.socket_forwarding.clone();
                if !socket_watch_paths.is_empty() {
                    sf.watch_paths = socket_watch_paths;
                    sf.enabled = true;
                }
                if let Some(prefix) = socket_container_path_prefix {
                    sf.container_path_prefix = Some(prefix);
                }
                if let Some(interval) = socket_scan_interval_ms {
                    sf.scan_interval_ms = interval;
                }
                sf
            };

            let config = HostConfig {
                control_port,
                data_port,
                bind_addr,
                no_docker_detect,
                exit_on_idle,
                browser_cmd,
                auth_token: resolved_auth_token,
                socket_forwarding,
                ..HostConfig::default()
            };

            let rt = match build_runtime() {
                Ok(rt) => rt,
                Err(code) => return code,
            };

            if let Err(e) = rt.block_on(dbr::host::run(config)) {
                error!(error = %e, "host daemon failed");
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }

        Command::ContainerDaemon {
            host_addr,
            scan_interval,
            exclude_ports,
            log_level,
            log_format,
            log_file,
            auth_token,
            auth_token_file,
        } => {
            init_tracing(&log_level, &log_format, log_file.as_deref());

            let mut config = match Config::from_env() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("configuration error: {e}");
                    return ExitCode::FAILURE;
                }
            };

            // CLI flags override env-based config
            if host_addr.is_some() {
                config.host_addr = host_addr;
            }
            config.scan_interval_ms = scan_interval;
            if !exclude_ports.is_empty() {
                config.exclude_ports = exclude_ports;
            }

            // Resolve auth token: CLI flag > CLI file > env var > env file >
            // container fallback (/run/secrets/dbr-auth-token) > default config path
            let resolved_token = {
                let token_file = auth_token_file.as_ref().map(std::path::Path::new);
                let container_fallback =
                    std::path::Path::new(dbr::auth::DEFAULT_CONTAINER_TOKEN_PATH);
                let default_path = dbr::auth::token_file_path().ok();

                // Try the standard resolution chain first
                match dbr::auth::resolve_token(
                    auth_token.as_deref(),
                    token_file,
                    container_fallback,
                ) {
                    Ok(token) => token,
                    Err(dbr::auth::AuthError::NoTokenSource { .. })
                    | Err(dbr::auth::AuthError::TokenNotFound { .. }) => {
                        // Container fallback not found — try default config path
                        if let Some(ref dp) = default_path {
                            match dbr::auth::resolve_token(None, None, dp) {
                                Ok(token) => token,
                                Err(_) => {
                                    info!(
                                        "no auth token found; connecting without authentication \
                                         (host may require --no-auth)"
                                    );
                                    String::new()
                                }
                            }
                        } else {
                            info!(
                                "no auth token found; connecting without authentication \
                                 (host may require --no-auth)"
                            );
                            String::new()
                        }
                    }
                    Err(e) => {
                        eprintln!("auth error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            };

            let rt = match build_runtime() {
                Ok(rt) => rt,
                Err(code) => return code,
            };

            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

            // Set up signal handler for graceful shutdown
            rt.spawn(async move {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    error!(error = %e, "failed to listen for ctrl-c");
                    return;
                }
                info!("received ctrl-c, shutting down");
                let _ = shutdown_tx.send(true);
            });

            if let Err(e) = rt.block_on(dbr::container::run(config, resolved_token, shutdown_rx)) {
                error!(error = %e, "container daemon failed");
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }

        Command::Open {
            url,
            control_port,
            auth_token,
            auth_token_file,
        } => {
            init_tracing("warn", "text", None);
            let token = resolve_cli_auth_token(&auth_token, &auth_token_file);
            run_async(browser::open_url(&url, control_port, &token))
        }

        Command::Status {
            control_port,
            host,
            json,
            auth_token,
            auth_token_file,
        } => {
            init_tracing("warn", "text", None);
            let token = resolve_cli_auth_token(&auth_token, &auth_token_file);
            run_async(async {
                let host = resolve_cli_host(host).await;
                run_status(host, control_port, json, &token).await
            })
        }

        Command::Forward {
            port,
            control_port,
            host,
            auth_token,
            auth_token_file,
        } => {
            init_tracing("warn", "text", None);
            let token = resolve_cli_auth_token(&auth_token, &auth_token_file);
            run_async(async {
                let host = resolve_cli_host(host).await;
                run_forward(host, port, control_port, &token).await
            })
        }

        Command::Unforward {
            port,
            control_port,
            host,
            auth_token,
            auth_token_file,
        } => {
            init_tracing("warn", "text", None);
            let token = resolve_cli_auth_token(&auth_token, &auth_token_file);
            run_async(async {
                let host = resolve_cli_host(host).await;
                run_unforward(host, port, control_port, &token).await
            })
        }

        Command::Ensure {
            control_port,
            data_port,
            host,
            auth_token,
            auth_token_file,
            no_auth,
        } => {
            init_tracing("warn", "text", None);
            run_async(async {
                let host = resolve_cli_host(host).await;
                dbr::host::ensure::run_ensure(
                    host,
                    control_port,
                    data_port,
                    no_auth,
                    auth_token,
                    auth_token_file,
                )
                .await
            })
        }
    }
}

/// Resolve an auth token for CLI commands (host-side).
///
/// Resolution chain: `--auth-token` flag > `--auth-token-file` flag >
/// `DCBRIDGE_AUTH_TOKEN` env > `DCBRIDGE_AUTH_TOKEN_FILE` env >
/// `~/.config/dbr/auth-token` default file.
///
/// Returns an empty string if no token is found (allows connecting to
/// `--no-auth` host daemons without requiring a token).
fn resolve_cli_auth_token(auth_token: &Option<String>, auth_token_file: &Option<String>) -> String {
    let token_file = auth_token_file.as_ref().map(std::path::Path::new);
    let default_path = match dbr::auth::token_file_path() {
        Ok(p) => p,
        Err(_) => return String::new(),
    };

    dbr::auth::resolve_token(auth_token.as_deref(), token_file, &default_path).unwrap_or_default()
}

/// Resolve the host daemon address for CLI commands.
///
/// Resolution chain (first match wins):
/// 1. Explicit `--host` flag
/// 2. `DCBRIDGE_HOST` environment variable
/// 3. `host.docker.internal` DNS (works inside containers)
/// 4. `127.0.0.1` (fallback for host-side usage)
async fn resolve_cli_host(host: Option<String>) -> IpAddr {
    // 1. Explicit flag
    if let Some(ref h) = host {
        if let Ok(ip) = h.parse::<IpAddr>() {
            return ip;
        }
        // Try DNS resolution for hostnames
        if let Ok(mut addrs) = tokio::net::lookup_host(format!("{h}:0")).await {
            if let Some(addr) = addrs.next() {
                return addr.ip();
            }
        }
        // If parsing and DNS both fail, the caller will get a connection
        // error with an actionable message.
    }

    // 2. DCBRIDGE_HOST env var
    if let Ok(env_host) = std::env::var("DCBRIDGE_HOST") {
        if !env_host.is_empty() {
            if let Ok(ip) = env_host.parse::<IpAddr>() {
                return ip;
            }
            if let Ok(mut addrs) = tokio::net::lookup_host(format!("{env_host}:0")).await {
                if let Some(addr) = addrs.next() {
                    debug!(host = %env_host, resolved = %addr.ip(), "resolved via DCBRIDGE_HOST");
                    return addr.ip();
                }
            }
        }
    }

    // 3. host.docker.internal DNS
    if let Ok(mut addrs) = tokio::net::lookup_host("host.docker.internal:0").await {
        if let Some(addr) = addrs.next() {
            debug!(resolved = %addr.ip(), "resolved via host.docker.internal");
            return addr.ip();
        }
    }

    // 4. Fallback
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

/// Connect to the host daemon control port, returning a connection or
/// a user-facing error message.
async fn connect_to_host(
    host: std::net::IpAddr,
    control_port: u16,
) -> Result<ControlConnection, String> {
    let addr: SocketAddr = (host, control_port).into();
    control::connect(addr).await.map_err(|_| {
        format!(
            "could not connect to host daemon at {addr}. \
             Is it running? Try `dbr ensure` first."
        )
    })
}

/// Register as a manual CLI client, returning an error on failure.
///
/// # Arguments
///
/// * `conn` — The control channel connection.
/// * `auth_token` — Authentication token to include in the Register message.
///   Empty string if no token is available (host may be running with `--no-auth`).
async fn register_cli_client(
    conn: &mut ControlConnection,
    auth_token: &str,
) -> Result<(), CliError> {
    conn.send(&Message::Register {
        container_id: "cli-manual".to_string(),
        hostname: "cli".to_string(),
        auth_token: auth_token.to_string(),
    })
    .await?;

    match conn.recv().await? {
        Message::RegisterAck { success: true } => Ok(()),
        _ => Err(CliError::RegistrationRejected),
    }
}

/// Connect to the host daemon and send a `ListRequest`, displaying the forward table.
///
/// If `json` is true, outputs the response as JSON. Otherwise, displays a
/// human-readable table.
async fn run_status(
    host: std::net::IpAddr,
    control_port: u16,
    json: bool,
    auth_token: &str,
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;

    // Status uses ListRequest which doesn't require registration,
    // but send auth token for future-proofing when auth is enforced
    // on all connections.
    let _ = auth_token; // reserved for future use
    conn.send(&Message::ListRequest).await?;

    match conn.recv().await? {
        Message::ListResponse { forwards } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&forwards)?);
            } else if forwards.is_empty() {
                println!("No active forwards.");
            } else {
                print_forward_table(&forwards);
            }
            Ok(())
        }
        other => Err(CliError::Connection(format!(
            "unexpected response from host daemon: {other:?}"
        ))),
    }
}

/// Print a human-readable table of active forwards.
fn print_forward_table(forwards: &[ForwardInfo]) {
    // Column headers
    println!(
        "{:<20} {:>5}   {:>9}  {:<12} Since",
        "Container", "Port", "Host Port", "Process"
    );

    for fwd in forwards {
        let process = fwd.process_name.as_deref().unwrap_or("-");
        let since = format_since(&fwd.since);
        println!(
            "{:<20} {:>5}   {:>9}  {:<12} {}",
            fwd.hostname, fwd.port, fwd.host_port, process, since
        );
    }
}

/// Format a Unix-epoch timestamp string into a human-readable "ago" string.
fn format_since(since: &str) -> String {
    let Ok(epoch_secs) = since.parse::<u64>() else {
        return since.to_string();
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if now < epoch_secs {
        return since.to_string();
    }

    let elapsed = now - epoch_secs;
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

/// Connect to the host daemon and send a manual `Forward` request.
///
/// Keeps the connection alive until Ctrl-C so the forward persists.
/// On exit, sends an `Unforward` to clean up.
async fn run_forward(
    host: std::net::IpAddr,
    port: u16,
    control_port: u16,
    auth_token: &str,
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;
    register_cli_client(&mut conn, auth_token).await?;

    conn.send(&Message::Forward {
        port,
        protocol: Protocol::Tcp,
        process_name: None,
        pid: None,
    })
    .await?;

    match conn.recv().await? {
        Message::ForwardAck {
            success: true,
            host_port,
            ..
        } => {
            println!("Forwarding port {port} → host port {host_port}");
            println!("Press Ctrl-C to stop forwarding.");
        }
        Message::ForwardAck { success: false, .. } => {
            return Err(CliError::Connection(format!(
                "host daemon failed to forward port {port}"
            )));
        }
        other => {
            return Err(CliError::Connection(format!(
                "unexpected response: {other:?}"
            )));
        }
    }

    // Keep the connection alive until Ctrl-C, responding to Pings
    // from the host daemon so the heartbeat doesn't expire.
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = conn.send(&Message::Unforward { port }).await;
                println!("Stopped forwarding port {port}.");
                return Ok(());
            }
            msg = conn.recv() => {
                match msg {
                    Ok(Message::Ping) => {
                        let _ = conn.send(&Message::Pong).await;
                    }
                    Ok(Message::ConnectRequest { conn_id, .. }) => {
                        // Manual forwards don't support proxying; reject
                        // immediately so the client gets a fast failure.
                        let _ = conn.send(&Message::ConnectFailed {
                            conn_id,
                            error: "manual forward does not support proxying".into(),
                        }).await;
                    }
                    Err(_) => {
                        return Err(CliError::Connection(
                            "lost connection to host daemon".into(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Connect to the host daemon and send a manual `Unforward` request.
///
/// Sends `Unforward` as the first message (no registration needed).
/// The host daemon handles this as a one-shot administrative command
/// that searches all containers for the port.
async fn run_unforward(
    host: std::net::IpAddr,
    port: u16,
    control_port: u16,
    auth_token: &str,
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;

    // Unforward is a one-shot administrative command that doesn't
    // require registration, but we include the token for future use.
    let _ = auth_token; // reserved for future use
    conn.send(&Message::Unforward { port }).await?;
    println!("Unforward request sent for port {port}");
    Ok(())
}

/// Check whether the binary was invoked via a `dbr-open` hardlink.
///
/// Inspects `argv[0]` and returns `true` if the file stem is `dbr-open`.
fn invoked_as_dbr_open() -> bool {
    std::env::args()
        .next()
        .and_then(|arg0| {
            std::path::Path::new(&arg0)
                .file_name()
                .and_then(|f| f.to_str())
                .map(|name| name == "dbr-open")
        })
        .unwrap_or(false)
}

/// Handle invocation as `dbr-open <URL>` (hardlink mode).
///
/// Parses the URL from remaining arguments and delegates to
/// [`browser::open_url`].
fn run_dbr_open() -> ExitCode {
    let url = match std::env::args().nth(1) {
        Some(url) => url,
        None => {
            eprintln!("usage: dbr-open <URL>");
            eprintln!("  Opens a URL in the host browser via the dbr host daemon.");
            eprintln!(
                "  Set BROWSER=dbr-open in your shell profile for automatic browser integration."
            );
            return ExitCode::FAILURE;
        }
    };

    init_tracing("warn", "text", None);
    let config = Config::from_env().unwrap_or_default();
    // dbr-open hardlink mode: resolve token from env/files (no CLI flags available)
    let token = resolve_cli_auth_token(&None, &None);
    run_async(browser::open_url(&url, config.control_port, &token))
}

/// Initialize the tracing subscriber with the given log level, format, and optional file.
///
/// Supported formats: `"text"` (default human-readable) and `"json"` (machine-parseable).
fn init_tracing(log_level: &str, log_format: &str, log_file: Option<&str>) {
    use tracing_subscriber::fmt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let use_json = log_format.eq_ignore_ascii_case("json");

    // Macro reduces the json/text branching to a single call-site.
    // The tracing_subscriber builder returns different concrete types
    // for `.json()` vs plain, so a macro is the simplest dedup.
    macro_rules! init_subscriber {
        ($builder:expr) => {
            if use_json {
                $builder.json().with_env_filter(filter).init();
            } else {
                $builder.with_env_filter(filter).init();
            }
        };
    }

    match log_file {
        Some(path) => {
            let file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "warning: could not open log file {path}: {e}, falling back to stderr"
                    );
                    init_subscriber!(fmt());
                    return;
                }
            };
            init_subscriber!(fmt().with_writer(file).with_ansi(false));
        }
        None => {
            init_subscriber!(fmt());
        }
    }
}
