use std::sync::Arc;

use agent_ops_core::HostConfig;
use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug)]
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Self {
        Self
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

async fn connect_to_bridge(
    bridge_addr: &str,
    bridge_token: &str,
    ca_cert_path: &str,
    insecure: bool,
) -> Result<quinn::Connection> {
    let tls_config = if insecure {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification::new()))
            .with_no_client_auth()
    } else {
        let ca_pem = std::fs::read(ca_cert_path)
            .with_context(|| format!("failed to read CA cert: {}", ca_cert_path))?;

        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
            let cert = cert?;
            roots.add(cert)?;
        }

        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

    let conn = endpoint
        .connect(bridge_addr.parse()?, "rmux-bridge")?
        .await?;

    let (mut auth_send, mut auth_recv) = conn.open_bi().await?;
    auth_send.write_all(b"AUTH").await?;
    auth_send
        .write_all(&(bridge_token.len() as u32).to_le_bytes())
        .await?;
    auth_send.write_all(bridge_token.as_bytes()).await?;
    auth_send.finish()?;

    let mut response = [0u8; 32];
    let n = auth_recv.read(&mut response).await?.unwrap_or(0);
    if n < 2 || &response[..n] != b"OK\n" {
        anyhow::bail!("bridge auth failed");
    }

    Ok(conn)
}

pub async fn connect(
    config: &HostConfig,
    ca_cert_path: &str,
    insecure: bool,
    session_name: &str,
    pane_id: &str,
    readonly: bool,
) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path, insecure)
        .await
        .context("failed to connect to bridge")?;

    let (cols, rows) = crossterm::terminal::size().context("failed to get terminal size")?;

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    ctrl_send.write_all(&[0x06]).await?;
    crate::protocol::write_attach_request(&mut ctrl_send, session_name, pane_id, cols, rows)
        .await?;

    let scrollback = crate::protocol::read_attached_response(&mut ctrl_recv).await?;

    let (mut data_send, mut data_recv) = conn.open_bi().await?;
    data_send.write_all(&[0x07]).await?;

    enable_raw_mode()?;

    if !scrollback.is_empty() {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&scrollback).await?;
        stdout.flush().await?;
    }

    let ctrl_send = Arc::new(tokio::sync::Mutex::new(ctrl_send));

    let result = if readonly {
        tokio::select! {
            r = quic_to_stdout(&mut data_recv) => r,
            r = resize_watcher(ctrl_send.clone()) => r,
        }
    } else {
        tokio::select! {
            r = stdin_to_quic(&mut data_send) => r,
            r = quic_to_stdout(&mut data_recv) => r,
            r = resize_watcher(ctrl_send.clone()) => r,
        }
    };

    disable_raw_mode()?;
    crate::protocol::write_detach(&mut *ctrl_send.lock().await)
        .await
        .ok();

    result
}

async fn stdin_to_quic(send: &mut quinn::SendStream) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
    }
    Ok(())
}

async fn quic_to_stdout(recv: &mut quinn::RecvStream) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    while let Some(n) = recv.read(&mut buf).await? {
        stdout.write_all(&buf[..n]).await?;
        stdout.flush().await?;
    }
    Ok(())
}

async fn resize_watcher(
    ctrl_send: Arc<tokio::sync::Mutex<quinn::SendStream>>,
) -> Result<()> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;

    let mut reader = EventStream::new();
    while let Some(event) = reader.next().await {
        if let Ok(Event::Resize(cols, rows)) = event {
            let mut send = ctrl_send.lock().await;
            crate::protocol::write_resize(&mut send, cols, rows).await?;
        }
    }
    Ok(())
}

pub async fn list_sessions(config: &HostConfig, ca_cert_path: &str, insecure: bool) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path, insecure).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({ "type": "list_sessions" });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = response["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to list sessions: {}", err);
    }

    if let Some(sessions) = response.get("sessions").and_then(|s| s.as_array()) {
        if sessions.is_empty() {
            println!("No active sessions on {}", config.name);
            return Ok(());
        }
        println!("{:<30} HOST", "SESSION");
        println!("{}", "-".repeat(50));
        for session in sessions {
            println!(
                "{:<30} {}",
                session["session_name"].as_str().unwrap_or("-"),
                config.name,
            );
        }
        println!("\n{} session(s) on {}", sessions.len(), config.name);
    } else {
        println!("No sessions found on {}", config.name);
    }

    Ok(())
}
