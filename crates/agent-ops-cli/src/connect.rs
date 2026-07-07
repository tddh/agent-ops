use std::sync::Arc;

use agent_ops_core::HostConfig;
use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn connect_to_bridge(
    bridge_addr: &str,
    bridge_token: &str,
    ca_cert_path: &str,
) -> Result<quinn::Connection> {
    let ca_pem = std::fs::read(ca_cert_path)
        .with_context(|| format!("failed to read CA cert: {}", ca_cert_path))?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
        let cert = cert?;
        roots.add(cert)?;
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_tls)));

    let conn = endpoint
        .connect(bridge_addr.parse()?, "rmux-bridge")?
        .await?;

    let (mut auth_send, mut auth_recv) = conn.open_bi().await?;
    let auth_frame = format!("AUTH{}\n{}", bridge_token.len(), bridge_token);
    auth_send.write_all(auth_frame.as_bytes()).await?;
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
    session_name: &str,
    pane_id: &str,
    readonly: bool,
) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path)
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

pub async fn list_sessions(config: &HostConfig, ca_cert_path: &str) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({ "type": "session_list" });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if let Some(sessions) = response.get("sessions").and_then(|s| s.as_array()) {
        println!("{:<20} {:<10} STATUS", "SESSION", "COUNT");
        println!("{}", "-".repeat(50));
        for session in sessions {
            println!(
                "{:<20} {:<10} ok",
                session["session_name"].as_str().unwrap_or("-"),
                sessions.len(),
            );
        }
    }

    Ok(())
}
