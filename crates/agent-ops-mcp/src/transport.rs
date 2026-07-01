//! TLS transport layer for connecting the MCP server to remote rmux-bridge
//! instances. Supports CA-verified and insecure (skip verification) TLS
//! handshakes, plus token-based authentication.

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use std::io::BufReader;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::sleep;
use tokio_rustls::TlsConnector;

/// Connects to a bridge via TCP/TLS, authenticates with the given token,
/// and returns the encrypted stream.
pub async fn connect_to_bridge(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    insecure: bool,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(bridge_addr)
        .await
        .with_context(|| format!("failed to connect to bridge at {}", bridge_addr))?;

    let tls_config = build_tls_client_config(ca_cert_path, insecure)?;
    let connector = TlsConnector::from(Arc::new(tls_config));
    let hostname = bridge_addr.split(':').next().unwrap_or("localhost");
    let server_name = ServerName::try_from(hostname)?.to_owned();

    let mut tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| anyhow::anyhow!("TLS handshake failed: {e}"))?;

    send_auth_frame(&mut tls_stream, auth_token).await?;

    let mut response = [0u8; 3];
    tokio::io::AsyncReadExt::read_exact(&mut tls_stream, &mut response).await?;

    if &response != b"OK\n" {
        anyhow::bail!("bridge authentication failed");
    }

    tracing::info!("connected and authenticated to bridge at {}", bridge_addr);
    Ok(tls_stream)
}

/// Like `connect_to_bridge` but retries up to `max_retries` times with
/// exponential backoff before giving up.
#[allow(dead_code)]
pub async fn connect_to_bridge_with_retry(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    max_retries: u32,
    insecure: bool,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut attempt = 0;
    loop {
        match connect_to_bridge(bridge_addr, auth_token, ca_cert_path, insecure).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt < max_retries => {
                attempt += 1;
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                tracing::warn!(
                    "bridge connection failed (attempt {}/{}), retrying in {:?}: {}",
                    attempt,
                    max_retries,
                    delay,
                    e
                );
                sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn build_tls_client_config(
    ca_cert_path: Option<&str>,
    insecure: bool,
) -> Result<rustls::ClientConfig> {
    if let Some(ca_path) = ca_cert_path {
        let ca_bytes = std::fs::read(ca_path)
            .with_context(|| format!("failed to read CA cert: {}", ca_path))?;
        let mut reader = BufReader::new(ca_bytes.as_slice());
        let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut reader)
            .filter_map(|r| r.ok())
            .collect();
        if certs.is_empty() {
            anyhow::bail!("no valid certificates found in {}", ca_path);
        }

        let mut root_store = rustls::RootCertStore::empty();
        for cert in certs {
            root_store
                .add(cert)
                .with_context(|| format!("failed to add CA cert from {}", ca_path))?;
        }
        tracing::info!("TLS: verifying bridge certificate against CA: {}", ca_path);

        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth())
    } else if insecure {
        tracing::warn!(
            "TLS: --insecure flag set, skipping certificate verification (NOT for production)"
        );

        Ok(rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerification))
            .with_no_client_auth())
    } else {
        anyhow::bail!(
            "no CA cert provided. Either pass --ca-cert <path> or --insecure (not recommended)"
        );
    }
}

#[derive(Debug)]
struct SkipVerification;

impl ServerCertVerifier for SkipVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

async fn send_auth_frame(
    stream: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    token: &str,
) -> Result<()> {
    let token_bytes = token.as_bytes();
    stream.write_all(b"AUTH").await?;
    stream
        .write_all(&(token_bytes.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(token_bytes).await?;
    stream.flush().await?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// QUIC connection (file transfers)
// ══════════════════════════════════════════════════════════════════

/// Establish QUIC connection to bridge for file transfers.
/// Returns authenticated Connection + first stream's send/recv handles.
/// Use `host.bridge_addr` directly — TCP and UDP share port 9778 safely.
pub async fn connect_to_bridge_quic(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    insecure: bool,
) -> anyhow::Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let addr: std::net::SocketAddr = bridge_addr
        .parse()
        .with_context(|| format!("invalid bridge address: {}", bridge_addr))?;

    let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;

    let tls_config = build_quic_client_config(ca_cert_path, insecure)?;
    let mut client_config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(std::sync::Arc::new(tls_config))
            .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {e}"))?,
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    client_config.transport_config(std::sync::Arc::new(transport));
    endpoint.set_default_client_config(client_config);

    let server_name = bridge_addr.split(':').next().unwrap_or("localhost");
    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(addr, server_name)?,
    )
    .await
    .context("QUIC connect timeout")?
    .context("QUIC connection failed")?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .context("failed to open QUIC auth stream")?;

    send_auth_frame_quic(&mut send, auth_token).await?;

    let mut response = [0u8; 3];
    tokio::io::AsyncReadExt::read_exact(&mut recv, &mut response).await?;
    if &response != b"OK\n" {
        conn.close(1u32.into(), b"auth failed");
        anyhow::bail!("bridge QUIC authentication failed");
    }

    tracing::info!("QUIC connected and authenticated to {}", bridge_addr);

    let (json_send, json_recv) = conn
        .open_bi()
        .await
        .context("failed to open QUIC json stream")?;

    Ok((conn, json_send, json_recv))
}

fn build_quic_client_config(
    ca_cert_path: Option<&str>,
    insecure: bool,
) -> anyhow::Result<rustls::ClientConfig> {
    if let Some(ca_path) = ca_cert_path {
        let ca_bytes = std::fs::read(ca_path)
            .with_context(|| format!("failed to read CA cert: {}", ca_path))?;
        let mut reader = BufReader::new(ca_bytes.as_slice());
        let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut reader)
            .filter_map(|r| r.ok())
            .collect();
        if certs.is_empty() {
            anyhow::bail!("no valid certificates found in {}", ca_path);
        }
        let mut root_store = rustls::RootCertStore::empty();
        for cert in certs {
            root_store
                .add(cert)
                .with_context(|| format!("failed to add CA cert from {}", ca_path))?;
        }
        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth())
    } else if insecure {
        Ok(rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(SkipVerification))
            .with_no_client_auth())
    } else {
        anyhow::bail!("no CA cert provided")
    }
}

async fn send_auth_frame_quic(send: &mut quinn::SendStream, token: &str) -> anyhow::Result<()> {
    let token_bytes = token.as_bytes();
    tokio::io::AsyncWriteExt::write_all(send, b"AUTH").await?;
    tokio::io::AsyncWriteExt::write_all(send, &(token_bytes.len() as u32).to_le_bytes()).await?;
    tokio::io::AsyncWriteExt::write_all(send, token_bytes).await?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// Hybrid transport: QUIC-first with TCP/TLS fallback
// ══════════════════════════════════════════════════════════════════

pub enum BridgeStream {
    Tcp(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Quic {
        #[allow(dead_code)]
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
}

impl AsyncRead for BridgeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Tcp(s) => Pin::new(&mut **s).poll_read(cx, buf),
            BridgeStream::Quic { recv, .. } => Pin::new(recv).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for BridgeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            BridgeStream::Tcp(s) => Pin::new(&mut **s).poll_write(cx, buf),
            BridgeStream::Quic { send, .. } => {
                match Pin::new(send).poll_write(cx, buf) {
                    Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
                    Poll::Ready(Err(e)) => {
                        Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Tcp(s) => Pin::new(&mut **s).poll_flush(cx),
            BridgeStream::Quic { .. } => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Tcp(s) => Pin::new(&mut **s).poll_shutdown(cx),
            BridgeStream::Quic { send, .. } => Pin::new(send).poll_shutdown(cx),
        }
    }
}

/// QUIC-first connection with TCP/TLS fallback.
/// Tries QUIC (UDP) first for 0-RTT reconnection and stream multiplexing.
/// Falls back to TCP/TLS if QUIC is unavailable (firewall blocks UDP, etc.).
pub async fn connect_to_bridge_hybrid(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    max_retries: u32,
    insecure: bool,
) -> Result<BridgeStream> {
    // Try QUIC first
    match connect_to_bridge_quic(bridge_addr, auth_token, ca_cert_path, insecure).await {
        Ok((conn, send, recv)) => {
            tracing::info!("connected via QUIC to {}", bridge_addr);
            return Ok(BridgeStream::Quic { conn, send, recv });
        }
        Err(e) => {
            tracing::debug!("QUIC connect failed (will try TCP): {}", e);
        }
    }

    // Fall back to TCP/TLS with retry
    let mut attempt = 0;
    loop {
        match connect_to_bridge(bridge_addr, auth_token, ca_cert_path, insecure).await {
            Ok(stream) => {
                tracing::info!("connected via TCP/TLS to {}", bridge_addr);
                return Ok(BridgeStream::Tcp(Box::new(stream)));
            }
            Err(e) if attempt < max_retries => {
                attempt += 1;
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                tracing::warn!(
                    "TCP connect failed (attempt {}/{}), retrying in {:?}: {}",
                    attempt, max_retries, delay, e
                );
                sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}
