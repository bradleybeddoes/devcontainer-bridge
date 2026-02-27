//! Configuration for the devcontainer-bridge daemons.
//!
//! Configuration values are loaded with the following precedence (highest first):
//! 1. CLI flags (handled by clap, not this module)
//! 2. Environment variables (`DCBRIDGE_HOST`, `DCBRIDGE_HOST_PORT`, etc.)
//! 3. Config file (`~/.config/dbr/config.toml`)
//! 4. Compiled-in defaults

use std::env;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

/// Default control channel port.
pub const DEFAULT_CONTROL_PORT: u16 = 19285;

/// Default data channel port.
pub const DEFAULT_DATA_PORT: u16 = 19286;

/// Default scan interval in milliseconds.
pub const DEFAULT_SCAN_INTERVAL_MS: u64 = 1000;

/// Default log level.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default log format.
pub const DEFAULT_LOG_FORMAT: &str = "text";

/// Errors that can occur when loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// An environment variable contained an invalid value.
    #[error("invalid value for {name}: {message}")]
    InvalidValue {
        /// The name of the configuration field.
        name: String,
        /// Description of what went wrong.
        message: String,
    },

    /// A port number was out of the valid range.
    #[error("port {value} is out of valid range (1-65535)")]
    InvalidPort {
        /// The invalid port value.
        value: String,
    },

    /// Failed to read or parse the config file.
    #[error("config file error: {0}")]
    ConfigFile(String),
}

/// TOML config file structure at `~/.config/dbr/config.toml`.
///
/// All fields are optional; missing fields retain their default values.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    /// TCP port for the control channel.
    control_port: Option<u16>,
    /// TCP port for the data channel.
    data_port: Option<u16>,
    /// Host address for the container daemon.
    host_addr: Option<String>,
    /// Scan interval in milliseconds.
    scan_interval_ms: Option<u64>,
    /// Logging level.
    log_level: Option<String>,
    /// Log format (text or json).
    log_format: Option<String>,
    /// Log file path.
    log_file: Option<String>,
    /// Ports to exclude from forwarding.
    exclude_ports: Option<Vec<u16>>,
    /// Ports to include (allowlist mode).
    include_ports: Option<Vec<u16>>,
    /// Disable authentication (host daemon only).
    no_auth: Option<bool>,
}

/// Runtime configuration for both host and container daemons.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port for the control channel (default: 19285).
    pub control_port: u16,
    /// TCP port for the data channel (default: 19286).
    pub data_port: u16,
    /// Host address for the container daemon to connect to.
    /// Resolved at runtime via: CLI flag → env var → DNS → gateway.
    pub host_addr: Option<String>,
    /// Interval between port scans in milliseconds (container daemon only).
    pub scan_interval_ms: u64,
    /// Logging level (trace, debug, info, warn, error).
    pub log_level: String,
    /// Log format: "text" (default) or "json".
    pub log_format: String,
    /// Optional file path for log output.
    pub log_file: Option<String>,
    /// Ports to never forward.
    pub exclude_ports: Vec<u16>,
    /// If non-empty, only forward these ports.
    pub include_ports: Vec<u16>,
    /// Disable authentication on the host daemon control channel.
    pub no_auth: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            control_port: DEFAULT_CONTROL_PORT,
            data_port: DEFAULT_DATA_PORT,
            host_addr: None,
            scan_interval_ms: DEFAULT_SCAN_INTERVAL_MS,
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            log_format: DEFAULT_LOG_FORMAT.to_owned(),
            log_file: None,
            exclude_ports: Vec::new(),
            include_ports: Vec::new(),
            no_auth: false,
        }
    }
}

impl Config {
    /// Create a configuration by layering config file → environment variables over defaults.
    ///
    /// Reads the config file at `~/.config/dbr/config.toml` (if present),
    /// then overlays the following environment variables:
    /// - `DCBRIDGE_HOST` → [`Config::host_addr`]
    /// - `DCBRIDGE_HOST_PORT` → [`Config::control_port`]
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the config file or an environment variable
    /// contains an invalid value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::load(config_file_path(), |key| env::var(key).ok())
    }

    /// Load configuration from an optional file path and an env var lookup function.
    ///
    /// This is the internal implementation used by [`Config::from_env`] and is
    /// exposed for testability.
    fn load<F>(file_path: Option<PathBuf>, lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        // Start with defaults
        let mut config = Self::default();

        // Layer 1: Config file
        if let Some(ref path) = file_path {
            if path.exists() {
                let contents = std::fs::read_to_string(path)
                    .map_err(|e| ConfigError::ConfigFile(format!("{}: {e}", path.display())))?;
                let file_cfg: FileConfig = toml::from_str(&contents)
                    .map_err(|e| ConfigError::ConfigFile(format!("{}: {e}", path.display())))?;
                apply_file_config(&mut config, &file_cfg)?;
            }
        }

        // Layer 2: Environment variables (override file)
        if let Some(host) = lookup("DCBRIDGE_HOST") {
            if !host.is_empty() {
                config.host_addr = Some(host);
            }
        }

        if let Some(port_str) = lookup("DCBRIDGE_HOST_PORT") {
            if !port_str.is_empty() {
                let port: u16 = port_str
                    .parse()
                    .map_err(|_| ConfigError::InvalidPort { value: port_str })?;
                if port == 0 {
                    return Err(ConfigError::InvalidPort {
                        value: "0".to_owned(),
                    });
                }
                config.control_port = port;
            }
        }

        Ok(config)
    }

    /// Create a configuration using only environment variables (no config file).
    ///
    /// This is a convenience for tests and backwards compatibility.
    #[cfg(test)]
    fn from_env_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::load(None, lookup)
    }
}

/// Apply values from a parsed config file onto a [`Config`].
fn apply_file_config(config: &mut Config, file: &FileConfig) -> Result<(), ConfigError> {
    if let Some(v) = file.control_port {
        if v == 0 {
            return Err(ConfigError::InvalidPort {
                value: "0".to_owned(),
            });
        }
        config.control_port = v;
    }
    if let Some(v) = file.data_port {
        if v == 0 {
            return Err(ConfigError::InvalidPort {
                value: "0".to_owned(),
            });
        }
        config.data_port = v;
    }
    if let Some(v) = file.scan_interval_ms {
        config.scan_interval_ms = v;
    }
    if let Some(ref v) = file.host_addr {
        if !v.is_empty() {
            config.host_addr = Some(v.clone());
        }
    }
    if let Some(ref v) = file.log_level {
        config.log_level.clone_from(v);
    }
    if let Some(ref v) = file.log_format {
        config.log_format.clone_from(v);
    }
    if file.log_file.is_some() {
        config.log_file.clone_from(&file.log_file);
    }
    if let Some(ref v) = file.exclude_ports {
        config.exclude_ports.clone_from(v);
    }
    if let Some(ref v) = file.include_ports {
        config.include_ports.clone_from(v);
    }
    if let Some(v) = file.no_auth {
        config.no_auth = v;
    }
    Ok(())
}

/// Return the path to the config file: `~/.config/dbr/config.toml`.
///
/// Returns `None` if the `HOME` environment variable is not set.
fn config_file_path() -> Option<PathBuf> {
    env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("dbr")
            .join("config.toml")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn default_values_are_correct() {
        let config = Config::default();
        assert_eq!(config.control_port, 19285);
        assert_eq!(config.data_port, 19286);
        assert_eq!(config.scan_interval_ms, 1000);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.log_format, "text");
        assert!(config.host_addr.is_none());
        assert!(config.log_file.is_none());
        assert!(config.exclude_ports.is_empty());
        assert!(config.include_ports.is_empty());
        assert!(!config.no_auth);
    }

    #[test]
    fn from_env_returns_defaults_when_no_vars_set() {
        let config = Config::from_env_lookup(make_lookup(&[])).unwrap();
        assert_eq!(config.control_port, DEFAULT_CONTROL_PORT);
        assert!(config.host_addr.is_none());
    }

    #[test]
    fn from_env_reads_host() {
        let config =
            Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST", "host.docker.internal")]))
                .unwrap();
        assert_eq!(config.host_addr.as_deref(), Some("host.docker.internal"));
    }

    #[test]
    fn from_env_reads_host_port() {
        let config =
            Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST_PORT", "19300")])).unwrap();
        assert_eq!(config.control_port, 19300);
    }

    #[test]
    fn from_env_rejects_invalid_port() {
        let result = Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST_PORT", "notanumber")]));
        assert!(result.is_err());
    }

    #[test]
    fn from_env_rejects_zero_port() {
        let result = Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST_PORT", "0")]));
        assert!(result.is_err());
    }

    #[test]
    fn from_env_ignores_empty_host() {
        let config = Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST", "")])).unwrap();
        assert!(config.host_addr.is_none());
    }

    #[test]
    fn from_env_ignores_empty_port() {
        let config = Config::from_env_lookup(make_lookup(&[("DCBRIDGE_HOST_PORT", "")])).unwrap();
        assert_eq!(config.control_port, DEFAULT_CONTROL_PORT);
    }

    #[test]
    fn load_from_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
control_port = 19300
data_port = 19301
host_addr = "myhost"
scan_interval_ms = 500
log_level = "debug"
log_format = "json"
log_file = "/tmp/dbr.log"
exclude_ports = [22, 5432]
include_ports = [8080, 3000]
"#,
        )
        .unwrap();

        let config = Config::load(Some(config_path), make_lookup(&[])).unwrap();
        assert_eq!(config.control_port, 19300);
        assert_eq!(config.data_port, 19301);
        assert_eq!(config.host_addr.as_deref(), Some("myhost"));
        assert_eq!(config.scan_interval_ms, 500);
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.log_format, "json");
        assert_eq!(config.log_file.as_deref(), Some("/tmp/dbr.log"));
        assert_eq!(config.exclude_ports, vec![22, 5432]);
        assert_eq!(config.include_ports, vec![8080, 3000]);
    }

    #[test]
    fn env_vars_override_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
control_port = 19300
host_addr = "fromfile"
"#,
        )
        .unwrap();

        let config = Config::load(
            Some(config_path),
            make_lookup(&[
                ("DCBRIDGE_HOST", "fromenv"),
                ("DCBRIDGE_HOST_PORT", "19400"),
            ]),
        )
        .unwrap();

        // Env vars win over file
        assert_eq!(config.control_port, 19400);
        assert_eq!(config.host_addr.as_deref(), Some("fromenv"));
    }

    #[test]
    fn partial_config_file_leaves_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "log_level = \"warn\"\n").unwrap();

        let config = Config::load(Some(config_path), make_lookup(&[])).unwrap();
        assert_eq!(config.control_port, DEFAULT_CONTROL_PORT);
        assert_eq!(config.data_port, DEFAULT_DATA_PORT);
        assert_eq!(config.log_level, "warn");
    }

    #[test]
    fn missing_config_file_is_ok() {
        let config = Config::load(
            Some(PathBuf::from("/nonexistent/config.toml")),
            make_lookup(&[]),
        )
        .unwrap();
        assert_eq!(config.control_port, DEFAULT_CONTROL_PORT);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid toml [[[").unwrap();

        let result = Config::load(Some(config_path), make_lookup(&[]));
        assert!(matches!(result, Err(ConfigError::ConfigFile(_))));
    }

    #[test]
    fn unknown_config_field_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "unknown_field = true\n").unwrap();

        let result = Config::load(Some(config_path), make_lookup(&[]));
        assert!(
            matches!(result, Err(ConfigError::ConfigFile(_))),
            "unknown config fields should be rejected"
        );
    }

    #[test]
    fn load_no_auth_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "no_auth = true\n").unwrap();

        let config = Config::load(Some(config_path), make_lookup(&[])).unwrap();
        assert!(config.no_auth);
    }

    #[test]
    fn no_auth_defaults_to_false() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "control_port = 19300\n").unwrap();

        let config = Config::load(Some(config_path), make_lookup(&[])).unwrap();
        assert!(!config.no_auth);
    }
}
