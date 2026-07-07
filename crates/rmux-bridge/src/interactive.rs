//! 交互式终端处理器：处理 QUIC 0x06（控制流）和 0x07（数据流）
//!
//! 参考：docs/connect-design.md

use anyhow::Result;
use quinn::{RecvStream, SendStream};
use rmux_sdk::{PaneOutputChunk, TerminalSizeSpec};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

use crate::protocol::ProtocolProxy;

/// 交互式会话状态（在控制流和数据流之间共享）
pub struct InteractiveSession {
    pub session_name: String,
    pub pane_id: String,
    #[allow(dead_code)]
    pub cols: u16,
    #[allow(dead_code)]
    pub rows: u16,
    pub exit_code: Option<i32>,
    pub exit_notify: Arc<Notify>,
}

const SCROLLBACK_LINES: usize = 50;

async fn read_u8(recv: &mut RecvStream) -> Result<u8> {
    let mut buf = [0u8; 1];
    recv.read_exact(&mut buf).await?;
    Ok(buf[0])
}

async fn read_u16_le(recv: &mut RecvStream) -> Result<u16> {
    let mut buf = [0u8; 2];
    recv.read_exact(&mut buf).await?;
    Ok(u16::from_le_bytes(buf))
}

async fn read_bytes(recv: &mut RecvStream, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn handle_interactive_control(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<ProtocolProxy>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    let msg_type = read_u8(&mut recv).await?;
    if msg_type != 0x01 {
        write_error(&mut send, 0x03, "expected Attach message first").await?;
        return Ok(());
    }
    let payload_len = read_u16_le(&mut recv).await? as usize;
    let payload = read_bytes(&mut recv, payload_len).await?;

    let (session_name, pane_id, cols, rows, _term) = parse_attach_payload(&payload)?;

    let _session = match proxy.get_session(&session_name).await {
        Ok(s) => s,
        Err(_) => {
            write_error(
                &mut send,
                0x01,
                &format!("session not found: {}", session_name),
            )
            .await?;
            return Ok(());
        }
    };

    let pane = match proxy.get_pane(&session_name, &pane_id).await {
        Ok(p) => p,
        Err(e) => {
            write_error(&mut send, 0x02, &format!("pane not found: {}", e)).await?;
            return Ok(());
        }
    };

    let snapshot = pane.snapshot().await?;
    let raw_text = snapshot.visible_text();
    let lines: Vec<&str> = raw_text.lines().collect();
    let recent_lines = if lines.len() > SCROLLBACK_LINES {
        &lines[lines.len() - SCROLLBACK_LINES..]
    } else {
        &lines
    };
    let mut scrollback = String::new();
    scrollback.push_str("\r\n\x1b[2m--- scrollback (last 50 lines) ---\x1b[0m\r\n");
    for line in recent_lines {
        scrollback.push_str(line);
        scrollback.push_str("\r\n");
    }
    scrollback.push_str("\x1b[2m--- end of scrollback ---\x1b[0m\r\n");
    let scrollback = scrollback.into_bytes();

    pane.resize(TerminalSizeSpec::new(cols, rows)).await?;

    let exit_notify = Arc::new(Notify::new());
    {
        let mut state = session_state.lock().await;
        *state = Some(InteractiveSession {
            session_name: session_name.clone(),
            pane_id: pane_id.clone(),
            cols,
            rows,
            exit_code: None,
            exit_notify: exit_notify.clone(),
        });
    }

    write_attached(&mut send, &scrollback).await?;

    loop {
        let msg_result = tokio::select! {
            r = read_u8(&mut recv) => Some(r),
            _ = exit_notify.notified() => None,
        };

        let msg_type = match msg_result {
            Some(Ok(t)) => t,
            Some(Err(_)) | None => {
                if let Some(exit_code) =
                    session_state.lock().await.as_ref().and_then(|s| s.exit_code)
                {
                    write_process_exited(&mut send, exit_code).await?;
                    tracing::info!(
                        "process exited in {}/{}: code={}",
                        session_name,
                        pane_id,
                        exit_code
                    );
                }
                break;
            }
        };

        let payload_len = read_u16_le(&mut recv).await? as usize;
        let payload = read_bytes(&mut recv, payload_len).await?;

        match msg_type {
            0x02 => {
                let new_cols = u16::from_le_bytes([payload[0], payload[1]]);
                let new_rows = u16::from_le_bytes([payload[2], payload[3]]);
                pane.resize(TerminalSizeSpec::new(new_cols, new_rows))
                    .await?;
                tracing::debug!("resize: {}x{}", new_cols, new_rows);
            }
            0x03 => {
                tracing::info!("client detached from {}/{}", session_name, pane_id);
                break;
            }
            _ => {
                tracing::warn!("unknown control message type: 0x{:02x}", msg_type);
            }
        }
    }

    Ok(())
}

pub async fn handle_interactive_data(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<ProtocolProxy>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    let (session_name, pane_id) = {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            if let Some(info) = session_state.lock().await.as_ref() {
                break (info.session_name.clone(), info.pane_id.clone());
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for control stream (0x06) to attach");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };

    let pane = proxy.get_pane(&session_name, &pane_id).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    let output_thread = {
        let pane = pane.clone();
        let session_state = session_state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("output thread runtime");
            rt.block_on(async move {
                let mut output_stream = match pane.output_stream().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                loop {
                    match output_stream.poll_once().await {
                        Ok(chunks) => {
                            for chunk in chunks {
                                if let PaneOutputChunk::Bytes { bytes, .. } = chunk {
                                    let mut b = bytes;
                                    loop {
                                        match tx.try_send(b) {
                                            Ok(()) => break,
                                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                                            Err(tokio::sync::mpsc::error::TrySendError::Full(ret)) => {
                                                b = ret;
                                                std::thread::sleep(std::time::Duration::from_micros(100));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            let mut state = session_state.blocking_lock();
                            if let Some(ref mut s) = *state {
                                s.exit_code = Some(0);
                                s.exit_notify.notify_one();
                            }
                            return;
                        }
                    }
                }
            });
        })
    };

    let mut input_task = {
        let pane = pane.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Some(n) = recv.read(&mut buf).await? {
                let keys = String::from_utf8_lossy(&buf[..n]).into_owned();
                pane.send_key(&keys).await?;
            }
            Ok::<_, anyhow::Error>(())
        })
    };

    loop {
        tokio::select! {
            maybe_bytes = rx.recv() => {
                match maybe_bytes {
                    Some(bytes) => { send.write_all(&bytes).await?; }
                    None => break,
                }
            }
            _ = &mut input_task => {
                break;
            }
        }
    }

    output_thread.join().ok();
    Ok(())
}

fn parse_attach_payload(data: &[u8]) -> Result<(String, String, u16, u16, String)> {
    let mut offset = 0;

    let session_name_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;
    let session_name = String::from_utf8(data[offset..offset + session_name_len].to_vec())?;
    offset += session_name_len;

    let pane_id_len = data[offset] as usize;
    offset += 1;
    let pane_id = String::from_utf8(data[offset..offset + pane_id_len].to_vec())?;
    offset += pane_id_len;

    let cols = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;
    let rows = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;

    let term_len = data[offset] as usize;
    offset += 1;
    let term = String::from_utf8(data[offset..offset + term_len].to_vec())?;

    Ok((session_name, pane_id, cols, rows, term))
}

async fn write_attached(send: &mut SendStream, scrollback: &[u8]) -> Result<()> {
    send.write_all(&[0x81]).await?;
    let payload_len = 4 + scrollback.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&(scrollback.len() as u32).to_le_bytes()).await?;
    send.write_all(scrollback).await?;
    Ok(())
}

async fn write_error(send: &mut SendStream, code: u8, message: &str) -> Result<()> {
    send.write_all(&[0x82]).await?;
    let payload_len = 1 + 2 + message.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&[code]).await?;
    send.write_all(&(message.len() as u16).to_le_bytes()).await?;
    send.write_all(message.as_bytes()).await?;
    Ok(())
}

async fn write_process_exited(send: &mut SendStream, exit_code: i32) -> Result<()> {
    send.write_all(&[0x83]).await?;
    send.write_all(&4u16.to_le_bytes()).await?;
    send.write_all(&exit_code.to_le_bytes()).await?;
    Ok(())
}
