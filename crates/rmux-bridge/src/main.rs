mod auth;
mod config;
mod files;
mod protocol;
mod proxy;
mod tls;

use clap::Parser;
use futures::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_yamux::{Config, Session, StreamHandle};

use crate::protocol::ProtocolProxy;

async fn handle_stream(mut stream: StreamHandle) -> anyhow::Result<()> {
    let mut type_buf = [0u8; 1];
    stream.read_exact(&mut type_buf).await?;
    match type_buf[0] {
        0x02 => files::handle_upload_stream(stream).await,
        0x03 => files::handle_download_stream(stream).await,
        0x04 => files::handle_batch_upload_stream(stream).await,
        t => {
            tracing::warn!("unknown stream type: 0x{:02x}", t);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // quinn needs explicit crypto provider in musl builds
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = config::BridgeConfig::parse();
    tracing::info!("rmux-bridge starting on {}", config.listen_addr);

    let conn_limit = if config.max_connections > 0 {
        Some(Arc::new(Semaphore::new(config.max_connections)))
    } else {
        None
    };

    let tls_config = tls::load_tls_server_config(&config.tls_cert, &config.tls_key)?;
    let acceptor = TlsAcceptor::from(tls_config);

    let health_port = config
        .listen_addr
        .split(':')
        .next_back()
        .and_then(|p| p.parse::<u16>().ok())
        .map(|p| p + 1)
        .unwrap_or(9779);
    let health_addr = format!("127.0.0.1:{}", health_port);

    tokio::spawn(async move {
        let listener = match TcpListener::bind(&health_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("failed to bind health check server: {}", e);
                return;
            }
        };
        tracing::info!("health check listening on {}", health_addr);

        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOK";
                    let _ = stream.write_all(response).await;
                });
            }
        }
    });

    // ─── QUIC file transfer listener ───
    let quic_config = config.clone();
    let quic_conn_limit_pre = conn_limit.clone();
    tokio::spawn(async move {
        let conn_limit = quic_conn_limit_pre;
        let tls_cfg = match tls::load_quic_server_config(&quic_config.tls_cert, &quic_config.tls_key) {
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

                if let Err(e) = auth::authenticate_quic(&mut auth_send, &mut auth_recv, &token).await {
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

                loop {
                    match conn.accept_bi().await {
                        Ok((send, recv)) => {
                            let proxy = protocol_proxy.clone();
                            tokio::spawn(async move {
                                if let Err(e) = files::handle_quic_stream(send, recv, proxy).await {
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

    let listener = TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("listening on {} (TLS)", config.listen_addr);

    let auth_token = Arc::new(config.auth_token.clone());
    let rmux_socket = Arc::new(config.rmux_socket.clone());
    let tcp_conn_limit = conn_limit.clone();

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("accept failed: {}", e);
                continue;
            }
        };

        tracing::info!("connection from {}", peer_addr);

        let acceptor = acceptor.clone();
        let auth_token = auth_token.clone();
        let rmux_socket = rmux_socket.clone();
        let tcp_lim = tcp_conn_limit.clone();

        tokio::spawn(async move {
            let _permit = if let Some(lim) = tcp_lim {
                Some(lim.acquire_owned().await.unwrap_or_else(|_| {
                    unreachable!("semaphore closed")
                }))
            } else {
                None
            };
            let mut tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TLS handshake failed from {}: {}", peer_addr, e);
                    return;
                }
            };

            if let Err(e) = auth::authenticate(&mut tls_stream, &auth_token).await {
                tracing::warn!("auth failed from {}: {}", peer_addr, e);
                return;
            }

            let mut first_byte = [0u8; 1];
            if let Err(e) = tls_stream.read_exact(&mut first_byte).await {
                tracing::warn!("read first byte from {} failed: {e}", peer_addr);
                return;
            }
            tracing::info!("first byte after auth: 0x{:02x}", first_byte[0]);

            if first_byte[0] == 0xFE {
                let config = Config::default();
                let mut session = Session::new_server(tls_stream, config);

                while let Some(stream) = session.next().await {
                    match stream {
                        Ok(s) => {
                            tokio::spawn(async move {
                                if let Err(e) = handle_stream(s).await {
                                    tracing::warn!("stream error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("yamux error: {}", e);
                            break;
                        }
                    }
                }
            } else {
                let protocol_proxy = match ProtocolProxy::connect(&rmux_socket).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("rmux connect failed: {}", e);
                        return;
                    }
                };
                if let Err(e) = proxy::proxy_legacy(first_byte[0], tls_stream, &protocol_proxy).await {
                    tracing::error!("proxy error from {}: {}", peer_addr, e);
                }
            }
        });
    }
}
