//! CLI subcommand definitions for the `dbr` binary.
//!
//! Uses clap derive macros to define all subcommands and their flags.

use clap::{Parser, Subcommand};

use std::net::IpAddr;

use dbr::config::{
    DEFAULT_CONTROL_PORT, DEFAULT_DATA_PORT, DEFAULT_LOG_FORMAT, DEFAULT_LOG_LEVEL,
    DEFAULT_SCAN_INTERVAL_MS,
};

/// Devcontainer Bridge — auto-forward ports and open browser URLs
/// between devcontainers and the host.
#[derive(Debug, Parser)]
#[command(name = "dbr", version, about)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the host-side daemon (binds control + data ports).
    ///
    /// By default, the bind address is auto-detected: if Docker is running,
    /// control and data ports bind to 0.0.0.0 (all interfaces) so containers
    /// can reach the host via Docker Desktop's gateway IP. If Docker is not
    /// detected, they bind to 127.0.0.1 (loopback only).
    /// Forwarded per-port listeners always bind to loopback only.
    /// Use --bind-addr to set an explicit address, or --no-docker-detect
    /// to skip detection and default to 127.0.0.1.
    #[command(name = "host-daemon")]
    HostDaemon {
        /// IP address to bind control and data listeners to.
        ///
        /// When omitted, the bind address is auto-detected based on whether
        /// Docker is running (0.0.0.0 if Docker detected, 127.0.0.1 otherwise).
        /// Forwarded per-port listeners always bind to loopback regardless.
        #[arg(long)]
        bind_addr: Option<IpAddr>,

        /// Skip Docker detection and bind to 127.0.0.1.
        ///
        /// When set, disables the automatic Docker detection that would bind
        /// to 0.0.0.0. Ignored if --bind-addr is explicitly provided.
        #[arg(long)]
        no_docker_detect: bool,

        /// Control channel port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Data channel port.
        #[arg(long, default_value_t = DEFAULT_DATA_PORT)]
        data_port: u16,

        /// Log level (trace, debug, info, warn, error).
        #[arg(long, default_value = DEFAULT_LOG_LEVEL)]
        log_level: String,

        /// Log format (text or json).
        #[arg(long, default_value = DEFAULT_LOG_FORMAT)]
        log_format: String,

        /// Optional log file path.
        #[arg(long)]
        log_file: Option<String>,

        /// Exit when the last container disconnects.
        #[arg(long)]
        exit_on_idle: bool,

        /// Custom browser command for opening URLs (overrides `open`/`xdg-open`).
        ///
        /// Useful for headless environments or testing. Set to `/usr/bin/true` to
        /// accept all OpenUrl requests without actually opening a browser.
        #[arg(long)]
        browser_cmd: Option<String>,

        /// Authentication token for the control channel.
        ///
        /// If not provided, the token is read from ~/.config/dbr/auth-token
        /// (generated automatically on first run).
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,

        /// Disable authentication (deprecated; allows unauthenticated access).
        #[arg(long)]
        no_auth: bool,
    },

    /// Run the container-side daemon (inside a devcontainer).
    #[command(name = "container-daemon")]
    ContainerDaemon {
        /// Host address to connect to (overrides DCBRIDGE_HOST and auto-detection).
        #[arg(long)]
        host_addr: Option<String>,

        /// Port scan interval in milliseconds.
        #[arg(long, default_value_t = DEFAULT_SCAN_INTERVAL_MS)]
        scan_interval: u64,

        /// Comma-separated list of ports to never forward.
        #[arg(long, value_delimiter = ',')]
        exclude_ports: Vec<u16>,

        /// Log level (trace, debug, info, warn, error).
        #[arg(long, default_value = DEFAULT_LOG_LEVEL)]
        log_level: String,

        /// Log format (text or json).
        #[arg(long, default_value = DEFAULT_LOG_FORMAT)]
        log_format: String,

        /// Optional log file path.
        #[arg(long)]
        log_file: Option<String>,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,
    },

    /// Show active port forwards across all containers.
    #[command(name = "status")]
    Status {
        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Host daemon address (IP or hostname).
        ///
        /// When omitted, auto-resolves via DCBRIDGE_HOST env var,
        /// then host.docker.internal DNS, then 127.0.0.1.
        #[arg(long)]
        host: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,
    },

    /// Manually forward a container port.
    #[command(name = "forward")]
    Forward {
        /// The port to forward.
        port: u16,

        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Host daemon address (IP or hostname).
        ///
        /// When omitted, auto-resolves via DCBRIDGE_HOST env var,
        /// then host.docker.internal DNS, then 127.0.0.1.
        #[arg(long)]
        host: Option<String>,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,
    },

    /// Manually remove a port forward.
    #[command(name = "unforward")]
    Unforward {
        /// The port to stop forwarding.
        port: u16,

        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Host daemon address (IP or hostname).
        ///
        /// When omitted, auto-resolves via DCBRIDGE_HOST env var,
        /// then host.docker.internal DNS, then 127.0.0.1.
        #[arg(long)]
        host: Option<String>,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,
    },

    /// Open a URL in the host browser via the host daemon.
    ///
    /// Sends the URL to the host daemon, which opens it using `open` (macOS)
    /// or `xdg-open` (Linux). Only http:// and https:// URLs are accepted.
    #[command(
        name = "open",
        after_help = "\
BROWSER INTEGRATION:
  To make tools inside a devcontainer open URLs on the host automatically,
  create a hardlink and set the BROWSER environment variable:

    ln /usr/local/bin/dbr /usr/local/bin/dbr-open
    export BROWSER=dbr-open

  Add the export to your shell profile (~/.zshrc, ~/.bashrc) so it persists.
  Most tools that open browsers (Node.js `open`, Python `webbrowser`, Rust
  `open` crate) respect the BROWSER env var.

  Optionally, symlink dbr-open as xdg-open for tools that call it directly:

    ln -sf /usr/local/bin/dbr-open /usr/local/bin/xdg-open

  The hardlink is typically created at install time by the devcontainer feature.
"
    )]
    Open {
        /// The URL to open (http:// or https:// only).
        url: String,

        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,
    },

    /// Start the host daemon if it is not already running.
    #[command(name = "ensure")]
    Ensure {
        /// Control channel port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Data channel port.
        #[arg(long, default_value_t = DEFAULT_DATA_PORT)]
        data_port: u16,

        /// Host daemon address (IP or hostname) for health check.
        ///
        /// When omitted, auto-resolves via DCBRIDGE_HOST env var,
        /// then host.docker.internal DNS, then 127.0.0.1.
        #[arg(long)]
        host: Option<String>,

        /// Authentication token for the control channel.
        #[arg(long)]
        auth_token: Option<String>,

        /// Path to a file containing the authentication token.
        #[arg(long)]
        auth_token_file: Option<String>,

        /// Disable authentication when spawning a new daemon.
        #[arg(long)]
        no_auth: bool,
    },
}
