//! CLI configuration parsed from command-line arguments. Defines the
//! listen address, RMUX socket path, TLS certificate/key paths, and
//! the authentication token.

use clap::Parser;
use std::path::PathBuf;

/// Command-line configuration for the rmux-bridge daemon.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rmux-bridge",
    version,
    about = "RMUX Bridge - TCP/TLS to Unix socket protocol-aware proxy"
)]
pub struct BridgeConfig {
    #[arg(long, default_value = "0.0.0.0:9778")]
    pub listen_addr: String,

    #[arg(long, default_value = "/tmp/rmux-1000/default")]
    pub rmux_socket: String,

    #[arg(long, default_value = "certs/bridge.crt")]
    pub tls_cert: PathBuf,

    #[arg(long, default_value = "certs/bridge.key")]
    pub tls_key: PathBuf,

    #[arg(long, env = "BRIDGE_AUTH_TOKEN")]
    pub auth_token: String,

    /// QUIC listen address for file transfers (UDP). Same port as TCP is fine
    /// since TCP and UDP use separate protocol stacks.
    #[arg(long, default_value = "0.0.0.0:9778", env = "QUIC_LISTEN_ADDR")]
    pub quic_listen_addr: String,

    /// Maximum concurrent connections (TCP + QUIC each). 0 = unlimited.
    #[arg(long, default_value = "256", env = "MAX_CONNECTIONS")]
    pub max_connections: usize,

    /// Log level: trace, debug, info, warn, error.
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    pub log_level: String,
}
