//! CLI subcommand definitions for the `dbr` binary.
//!
//! Uses clap derive macros to define all subcommands and their flags.

use clap::{Parser, Subcommand};

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
    /// Run the host-side daemon (binds control + data ports on loopback).
    #[command(name = "host-daemon")]
    HostDaemon {
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

        /// Keep running after the last container disconnects.
        #[arg(long)]
        no_exit_on_idle: bool,
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
    },

    /// Show active port forwards across all containers.
    #[command(name = "status")]
    Status {
        /// Host daemon address (host:port).
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Manually forward a container port.
    #[command(name = "forward")]
    Forward {
        /// The port to forward.
        port: u16,

        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,
    },

    /// Manually remove a port forward.
    #[command(name = "unforward")]
    Unforward {
        /// The port to stop forwarding.
        port: u16,

        /// Host daemon control port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,
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
    },
}
