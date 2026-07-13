mod auth;
mod config;
mod files;
mod interactive;
mod protocol;
mod proxy;
mod terminal_state;
mod tls;

use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::interactive::InteractiveSession;
use crate::protocol::ProtocolProxy;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::BridgeConfig::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level)),
        )
        .init();

    // quinn needs explicit crypto provider in musl builds
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing::info!("rmux-bridge starting on {}", config.quic_listen_addr);

    let conn_limit = if config.max_connections > 0 {
        Some(Arc::new(Semaphore::new(config.max_connections)))
    } else {
        None
    };

    // ─── QUIC file transfer listener ───
    let quic_config = config.clone();
    let quic_conn_limit_pre = conn_limit.clone();
    tokio::spawn(async move {
        let conn_limit = quic_conn_limit_pre;
        let tls_cfg =
            match tls::load_quic_server_config(&quic_config.tls_cert, &quic_config.tls_key) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("failed to load QUIC TLS config: {}", e);
                    return;
                }
            };
        let quic_addr: SocketAddr = match quic_config.quic_listen_addr.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("invalid QUIC listen addr: {}", e);
                return;
            }
        };
        let endpoint = match quinn::Endpoint::server(tls_cfg, quic_addr) {
            Ok(ep) => ep,
            Err(e) => {
                tracing::error!("failed to create QUIC endpoint: {}", e);
                return;
            }
        };
        tracing::info!("QUIC file transfer listening on {}", quic_addr);

        let auth_token = std::sync::Arc::new(quic_config.auth_token.clone());
        let quic_rmux_socket = Arc::new(quic_config.rmux_socket.clone());
        let quic_conn_limit = conn_limit.clone();

        while let Some(incoming) = endpoint.accept().await {
            let _permit = if let Some(ref lim) = quic_conn_limit {
                match lim.clone().acquire_owned().await {
                    Ok(p) => Some(p),
                    Err(_) => break,
                }
            } else {
                None
            };

            let token = auth_token.clone();
            let rmux_socket = quic_rmux_socket.clone();
            tokio::spawn(async move {
                let _permit = _permit;
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("QUIC connection failed: {}", e);
                        return;
                    }
                };

                let (mut auth_send, mut auth_recv) = match conn.accept_bi().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("QUIC accept_bi failed: {}", e);
                        return;
                    }
                };

                if let Err(e) =
                    auth::authenticate_quic(&mut auth_send, &mut auth_recv, &token).await
                {
                    tracing::warn!("QUIC auth failed: {}", e);
                    return;
                }

                let protocol_proxy = match ProtocolProxy::connect(&rmux_socket).await {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        tracing::error!("QUIC rmux connect failed: {}", e);
                        return;
                    }
                };

                let session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>> =
                    std::sync::Arc::new(tokio::sync::Mutex::new(None));

                loop {
                    match conn.accept_bi().await {
                        Ok((send, recv)) => {
                            let proxy = protocol_proxy.clone();
                            let state = session_state.clone();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    files::handle_quic_stream(send, recv, proxy, state).await
                                {
                                    tracing::warn!("QUIC stream error: {}", e);
                                }
                            });
                        }
                        Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
                        Err(quinn::ConnectionError::LocallyClosed) => break,
                        Err(e) => {
                            tracing::warn!("QUIC accept_bi error: {}", e);
                            break;
                        }
                    }
                }
            });
        }
    });
    // ─── end QUIC listener ───

    // Block forever — QUIC listener runs in background task
    std::future::pending::<anyhow::Result<()>>().await
}
