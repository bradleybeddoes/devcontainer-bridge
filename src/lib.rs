//! Devcontainer Bridge (`dbr`) — auto-forward ports and open browser URLs
//! between devcontainers and the host.
//!
//! This crate provides the core library for both the container-side and
//! host-side daemons, a JSON-line control protocol, and supporting utilities.

pub mod auth;
pub mod config;
pub mod container;
pub mod control;
pub mod host;
pub mod protocol;
