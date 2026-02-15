//! CLI entrypoint for the `dbr` (devcontainer-bridge) binary.
//!
//! Parses command-line arguments, sets up tracing, and dispatches to the
//! appropriate module.
//!
//! When invoked as `dbr-open` (via hardlink), the binary treats its first
//! positional argument as a URL and behaves like `dbr open <URL>`.

mod cli;

use std::net::SocketAddr;
use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info};

use thiserror::Error;

use dbr::config::Config;
use dbr::config::DEFAULT_CONTROL_PORT;
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
            no_exit_on_idle,
            browser_cmd,
        } => {
            init_tracing(&log_level, &log_format, log_file.as_deref());

            let config = HostConfig {
                control_port,
                data_port,
                bind_addr,
                no_docker_detect,
                exit_on_idle: !no_exit_on_idle,
                browser_cmd,
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

            if let Err(e) = rt.block_on(dbr::container::run(config, shutdown_rx)) {
                error!(error = %e, "container daemon failed");
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }

        Command::Open { url, control_port } => {
            init_tracing("warn", "text", None);
            run_async(browser::open_url(&url, control_port))
        }

        Command::Status {
            control_port,
            host,
            json,
        } => {
            init_tracing("warn", "text", None);
            run_async(run_status(host, control_port, json))
        }

        Command::Forward {
            port,
            control_port,
            host,
        } => {
            init_tracing("warn", "text", None);
            run_async(run_forward(host, port, control_port))
        }

        Command::Unforward {
            port,
            control_port,
            host,
        } => {
            init_tracing("warn", "text", None);
            run_async(run_unforward(host, port, control_port))
        }

        Command::Ensure {
            control_port,
            data_port,
            host,
        } => {
            init_tracing("warn", "text", None);
            run_async(dbr::host::ensure::run_ensure(host, control_port, data_port))
        }
    }
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
async fn register_cli_client(conn: &mut ControlConnection) -> Result<(), CliError> {
    conn.send(&Message::Register {
        container_id: "cli-manual".to_string(),
        hostname: "cli".to_string(),
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
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;

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
async fn run_forward(
    host: std::net::IpAddr,
    port: u16,
    control_port: u16,
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;
    register_cli_client(&mut conn).await?;

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
            Ok(())
        }
        Message::ForwardAck { success: false, .. } => Err(CliError::Connection(format!(
            "host daemon failed to forward port {port}"
        ))),
        other => Err(CliError::Connection(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Connect to the host daemon and send a manual `Unforward` request.
async fn run_unforward(
    host: std::net::IpAddr,
    port: u16,
    control_port: u16,
) -> Result<(), CliError> {
    let mut conn = connect_to_host(host, control_port)
        .await
        .map_err(CliError::Connection)?;
    register_cli_client(&mut conn).await?;

    conn.send(&Message::Unforward { port }).await?;
    println!("Removed forward for port {port}");
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
    run_async(browser::open_url(&url, DEFAULT_CONTROL_PORT))
}

/// Initialize the tracing subscriber with the given log level, format, and optional file.
///
/// Supported formats: `"text"` (default human-readable) and `"json"` (machine-parseable).
fn init_tracing(log_level: &str, log_format: &str, log_file: Option<&str>) {
    use tracing_subscriber::fmt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let use_json = log_format.eq_ignore_ascii_case("json");

    if let Some(path) = log_file {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("warning: could not open log file {path}: {e}, falling back to stderr");
                if use_json {
                    fmt().json().with_env_filter(filter).init();
                } else {
                    fmt().with_env_filter(filter).init();
                }
                return;
            }
        };

        if use_json {
            fmt()
                .json()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
        } else {
            fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
        }
    } else if use_json {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}
