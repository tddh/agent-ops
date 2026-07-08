use std::sync::Arc;

use agent_ops_core::HostConfig;
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::AsyncWriteExt;

#[derive(Debug, PartialEq, Eq)]
enum TerminalAction {
    Input(Vec<u8>),
    Resize(u16, u16),
    Detach,
    Ignore,
}

fn translate_terminal_event(event: Event) -> TerminalAction {
    match event {
        Event::Key(key_event) => translate_key_event(key_event),
        Event::Resize(cols, rows) => TerminalAction::Resize(cols, rows),
        _ => TerminalAction::Ignore,
    }
}

fn translate_key_event(key: KeyEvent) -> TerminalAction {
    if key.kind == KeyEventKind::Release {
        return TerminalAction::Ignore;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Check for Ctrl+\ (both as structured event and raw control character)
    // Crossterm may send Ctrl+\ as either:
    // 1. KeyCode::Char('\\') with KeyModifiers::CONTROL
    // 2. KeyCode::Char('\x1c') as a raw control character (0x1c = 28)
    if ctrl && key.code == KeyCode::Char('\\') {
        return TerminalAction::Detach;
    }
    if let KeyCode::Char(c) = key.code {
        if c == '\x1c' {
            return TerminalAction::Detach;
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let byte = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                TerminalAction::Input(vec![byte])
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                TerminalAction::Input(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => TerminalAction::Input(vec![b'\r']),
        KeyCode::Backspace => TerminalAction::Input(vec![0x7f]),
        KeyCode::Tab => TerminalAction::Input(vec![b'\t']),
        KeyCode::Esc => TerminalAction::Input(vec![0x1b]),
        KeyCode::Up => TerminalAction::Input(vec![0x1b, b'[', b'A']),
        KeyCode::Down => TerminalAction::Input(vec![0x1b, b'[', b'B']),
        KeyCode::Right => TerminalAction::Input(vec![0x1b, b'[', b'C']),
        KeyCode::Left => TerminalAction::Input(vec![0x1b, b'[', b'D']),
        KeyCode::Home => TerminalAction::Input(vec![0x1b, b'[', b'H']),
        KeyCode::End => TerminalAction::Input(vec![0x1b, b'[', b'F']),
        KeyCode::PageUp => TerminalAction::Input(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => TerminalAction::Input(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Delete => TerminalAction::Input(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::Insert => TerminalAction::Input(vec![0x1b, b'[', b'2', b'~']),
        _ => TerminalAction::Ignore,
    }
}

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

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(0))));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_tls));
    client_config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(client_config);

    let server_name = bridge_addr.split(':').next().unwrap_or(bridge_addr);
    let conn = endpoint
        .connect(bridge_addr.parse()?, server_name)?
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

    enable_raw_mode()?;

    if !scrollback.is_empty() {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&scrollback).await?;
        stdout.flush().await?;
    }

    let (mut data_send, mut data_recv) = conn.open_bi().await?;
    data_send.write_all(&[0x07]).await?;

    let ctrl_send = Arc::new(tokio::sync::Mutex::new(ctrl_send));

    let result = tokio::select! {
        r = terminal_event_loop(&mut data_send, ctrl_send.clone(), readonly) => r,
        r = quic_to_stdout(&mut data_recv) => r,
    };

    disable_raw_mode()?;
    crate::protocol::write_detach(&mut *ctrl_send.lock().await)
        .await
        .ok();

    result
}

/// 终端事件循环：使用 crossterm EventStream 作为唯一输入源
///
/// 社区最佳实践：避免混用 tokio::io::stdin() 和 EventStream，
/// 因为两者会竞争 stdin fd，导致按键丢失或延迟。
/// 此函数统一处理所有终端事件（按键、Resize、粘贴等），
/// 通过 translate_terminal_event 转换为协议动作后发送。
async fn terminal_event_loop(
    data_send: &mut quinn::SendStream,
    ctrl_send: Arc<tokio::sync::Mutex<quinn::SendStream>>,
    readonly: bool,
) -> Result<()> {
    use crossterm::event::EventStream;
    use futures::StreamExt;

    let mut event_stream = EventStream::new();

    while let Some(event_result) = event_stream.next().await {
        let event = event_result.context("event stream error")?;
        match translate_terminal_event(event) {
            TerminalAction::Input(bytes) => {
                if !readonly {
                    data_send.write_all(&bytes).await?;
                }
            }
            TerminalAction::Resize(cols, rows) => {
                let mut send = ctrl_send.lock().await;
                crate::protocol::write_resize(&mut send, cols, rows).await?;
            }
            TerminalAction::Detach => {
                return Ok(());
            }
            TerminalAction::Ignore => {}
        }
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

pub async fn find_lowest_pane(
    config: &HostConfig,
    ca_cert_path: &str,
    insecure: bool,
    session_name: &str,
) -> Result<String> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path, insecure).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({
        "type": "list_window_panes",
        "session_name": session_name,
        "window_index": 0,
    });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if !response.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let err = response["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to list panes: {}", err);
    }

    let panes = response
        .get("panes")
        .and_then(|p| p.as_array())
        .context("no panes in response")?;

    let smallest = panes
        .iter()
        .filter_map(|p| p.get("pane_id").and_then(|id| id.as_str()))
        .filter_map(|id| id.trim_start_matches('%').parse::<u32>().ok())
        .min()
        .context("no panes found in session")?;

    Ok(format!("%{}", smallest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn translates_plain_character_to_input_bytes() {
        let action = translate_terminal_event(Event::Key(key_event(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        assert_eq!(action, TerminalAction::Input(vec![b'a']));
    }

    #[test]
    fn translates_enter_to_carriage_return() {
        let action = translate_terminal_event(Event::Key(key_event(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        assert_eq!(action, TerminalAction::Input(vec![b'\r']));
    }

    #[test]
    fn translates_ctrl_backslash_to_detach() {
        let action = translate_terminal_event(Event::Key(key_event(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL,
        )));

        assert_eq!(action, TerminalAction::Detach);
    }

    #[test]
    fn translates_ctrl_backslash_raw_control_char_to_detach() {
        let action = translate_terminal_event(Event::Key(key_event(
            KeyCode::Char('\x1c'),
            KeyModifiers::NONE,
        )));

        assert_eq!(action, TerminalAction::Detach);
    }

    #[test]
    fn translates_resize_event_to_resize_action() {
        let action = translate_terminal_event(Event::Resize(120, 40));

        assert_eq!(action, TerminalAction::Resize(120, 40));
    }
}
