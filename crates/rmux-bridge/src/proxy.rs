//! Protocol-aware TLS proxy: receives framed JSON requests from the MCP server
//! over a TLS stream, dispatches them to the `ProtocolProxy`, and returns
//! framed JSON responses. Handles streaming uploads/downloads inline.

use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::protocol::ProtocolProxy;

const FILE_UPLOAD_FRAME: u8 = 0x03;
const FILE_DOWNLOAD_FRAME: u8 = 0x04;

const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MB
const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MB

/// Main event loop: reads length-prefixed JSON frames from `tls_stream`,
/// dispatches each request to `protocol_proxy`, and writes back the response.
/// Special handling for file upload/download streaming frames.
pub async fn proxy_protocol_aware<S>(tls_stream: S, protocol_proxy: &ProtocolProxy) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, writer) = tokio::io::split(tls_stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let mut len_buf = [0u8; 4];

    loop {
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            tracing::warn!("frame read length error: {e}");
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            let err = json!({"ok": false, "error": format!("frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE)});
            send_response(&writer, &err).await?;
            continue;
        }

        let mut buf = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut buf).await {
            tracing::warn!("frame read body error: {e}");
            break;
        }

        let request: serde_json::Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({"error": format!("invalid json: {}", e)});
                send_response(&writer, &err_resp).await?;
                continue;
            }
        };

        let req_type = request["type"].as_str().unwrap_or("");

        // file_upload: receive streaming chunks (multiple binary frames, zero-size = EOF)
        if req_type == "file_upload" {
            let remote_path = request["remote_path"].as_str().unwrap_or("");
            if remote_path.is_empty() {
                let err = json!({"ok": false, "error": "missing remote_path"});
                send_response(&writer, &err).await?;
                continue;
            }

            // 创建父目录
            if let Some(parent) = std::path::Path::new(remote_path).parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        tracing::warn!("failed to create directory {}: {e}", parent.display());
                        let err = json!({"ok": false, "error": format!("failed to create directory: {}", e)});
                        send_response(&writer, &err).await?;
                        continue;
                    }
                }
            }

            // 流式写入临时文件，避免内存累积
            let tmp_path = format!("{}.tmp.{}", remote_path, std::process::id());
            let mut file = match std::fs::File::create(&tmp_path) {
                Ok(f) => f,
                Err(e) => {
                    let err =
                        json!({"ok": false, "error": format!("failed to create temp file: {}", e)});
                    send_response(&writer, &err).await?;
                    continue;
                }
            };

            use std::io::Write;
            let mut total_written: u64 = 0;
            let mut upload_ok = true;
            let mut upload_err = String::new();

            loop {
                let mut frame_type = [0u8; 1];
                if reader.read_exact(&mut frame_type).await.is_err() {
                    upload_ok = false;
                    upload_err = "connection closed during upload".into();
                    break;
                }
                if frame_type[0] != FILE_UPLOAD_FRAME {
                    upload_ok = false;
                    upload_err = format!("expected file upload frame, got type {}", frame_type[0]);
                    break;
                }
                let mut size_buf = [0u8; 8];
                if reader.read_exact(&mut size_buf).await.is_err() {
                    upload_ok = false;
                    upload_err = "connection closed during chunk read".into();
                    break;
                }
                let chunk_size = u64::from_le_bytes(size_buf) as usize;
                if chunk_size == 0 {
                    break; // EOF
                }
                if chunk_size > MAX_CHUNK_SIZE {
                    upload_ok = false;
                    upload_err = format!(
                        "chunk too large: {} bytes (max {})",
                        chunk_size, MAX_CHUNK_SIZE
                    );
                    break;
                }
                let mut chunk = vec![0u8; chunk_size];
                if reader.read_exact(&mut chunk).await.is_err() {
                    upload_ok = false;
                    upload_err = "connection closed during chunk data".into();
                    break;
                }
                if let Err(e) = file.write_all(&chunk) {
                    upload_ok = false;
                    upload_err = format!("write failed: {}", e);
                    break;
                }
                total_written += chunk_size as u64;
            }

            drop(file);

            if upload_ok {
                // 原子重命名
                if let Err(e) = std::fs::rename(&tmp_path, remote_path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    let err = json!({"ok": false, "error": format!("rename failed: {}", e)});
                    send_response(&writer, &err).await?;
                } else {
                    let resp = json!({"ok": true, "path": remote_path, "size": total_written});
                    send_response(&writer, &resp).await?;
                }
            } else {
                let _ = std::fs::remove_file(&tmp_path);
                let err = json!({"ok": false, "error": upload_err});
                send_response(&writer, &err).await?;
            }
            continue;
        }

        // file_download: send binary data frame then JSON response
        if req_type == "file_download" {
            let remote_path = request["remote_path"].as_str().unwrap_or("");
            match ProtocolProxy::handle_file_download_open(remote_path) {
                Ok(mut file) => {
                    let meta = match file.metadata() {
                        Ok(m) => m,
                        Err(e) => {
                            let err = json!({"ok": false, "error": format!("failed to get file metadata: {}", e)});
                            send_response(&writer, &err).await?;
                            continue;
                        }
                    };
                    let file_size = meta.len();

                    // 发送 frame type + size
                    {
                        let mut w = writer.lock().await;
                        if w.write_all(&[FILE_DOWNLOAD_FRAME]).await.is_err() {
                            break;
                        }
                        if w.write_all(&file_size.to_le_bytes()).await.is_err() {
                            break;
                        }
                    }

                    // 流式分块发送
                    use std::io::Read;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut sent: u64 = 0;
                    loop {
                        let n = match file.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) => {
                                let err =
                                    json!({"ok": false, "error": format!("read error: {}", e)});
                                send_response(&writer, &err).await?;
                                break;
                            }
                        };
                        let mut w = writer.lock().await;
                        if w.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        sent += n as u64;
                    }

                    let resp = json!({"ok": true, "path": remote_path, "size": sent});
                    send_response(&writer, &resp).await?;
                }
                Err(err_resp) => {
                    send_response(&writer, &err_resp).await?;
                }
            }
            continue;
        }

        if req_type == "stream_subscribe" {
            let sn = request["session_name"].as_str().unwrap_or("");
            let pane_id = request["pane_id"].as_str().unwrap_or("");
            match protocol_proxy.subscribe_pane_output(sn, pane_id).await {
                Ok(mut out_stream) => {
                    let resp = json!({
                        "ok": true,
                        "stream_subscribed": true,
                        "pane_id": pane_id,
                    });
                    send_response(&writer, &resp).await?;

                    let writer_clone = writer.clone();
                    let pid = pane_id.to_string();
                    tokio::spawn(async move {
                        while let Some(text) = out_stream.rx.recv().await {
                            let mut w = writer_clone.lock().await;
                            if w.write_all(&[0x02]).await.is_err() {
                                break;
                            }
                            if w.write_all(&(pid.len() as u32).to_le_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if w.write_all(pid.as_bytes()).await.is_err() {
                                break;
                            }
                            if w.write_all(&(text.len() as u32).to_le_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if w.write_all(text.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    let err_resp = json!({"ok": false, "error": e.to_string()});
                    send_response(&writer, &err_resp).await?;
                }
            }
            continue;
        }

        let response = match req_type {
            "new_session" => {
                let name = request["name"].as_str().unwrap_or("agent-session");
                let detached = request["detached"].as_bool().unwrap_or(true);
                protocol_proxy.handle_new_session(name, detached).await
            }
            "list_sessions" => protocol_proxy.handle_list_sessions().await,
            "attach_session" => {
                let name = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_attach_session(name).await
            }
            "detach_session" => {
                let name = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_detach_session(name).await
            }
            "send_keys" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let keys = request["keys"].as_str().unwrap_or("");
                protocol_proxy.handle_send_keys(sn, pane_id, keys).await
            }
            "capture_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let max_lines = request["max_lines"].as_u64().map(|v| v as usize);
                protocol_proxy
                    .handle_capture_pane(sn, pane_id, max_lines)
                    .await
            }
            "wait_for_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let text = request["text"].as_str().unwrap_or("");
                let timeout = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy
                    .handle_wait_for_text(sn, pane_id, text, timeout)
                    .await
            }
            "wait_exit" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let timeout = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy.handle_wait_exit(sn, pane_id, timeout).await
            }
            "split_window" => {
                let session_name = request["session_name"].as_str().unwrap_or("");
                let dir = request["direction"].as_str().unwrap_or("horizontal");
                protocol_proxy.handle_split_window(session_name, dir).await
            }
            "spawn_command" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cmd = request["command"].as_str().unwrap_or("");
                let args: Vec<String> = request["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                protocol_proxy
                    .handle_spawn_command(sn, pane_id, cmd, &args)
                    .await
            }
            "shell_command" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cmd = request["command"].as_str().unwrap_or("");
                protocol_proxy.handle_shell_command(sn, pane_id, cmd).await
            }
            "respawn_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_respawn_pane(sn, pane_id).await
            }
            "broadcast_keys" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_ids: Vec<String> = request["pane_ids"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let keys = request["keys"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_broadcast_keys(sn, &pane_ids, keys)
                    .await
            }
            "cmd_escape" => {
                let args: Vec<String> = request["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                protocol_proxy.handle_cmd_escape(&args).await
            }
            "split_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let dir = request["direction"].as_str().unwrap_or("horizontal");
                protocol_proxy.handle_split_pane(sn, pane_id, dir).await
            }
            "resize_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cols = request["cols"].as_u64().unwrap_or(80) as u16;
                let rows = request["rows"].as_u64().unwrap_or(24) as u16;
                protocol_proxy
                    .handle_resize_pane(sn, pane_id, cols, rows)
                    .await
            }
            "send_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let text = request["text"].as_str().unwrap_or("");
                protocol_proxy.handle_send_text(sn, pane_id, text).await
            }
            "set_pane_title" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let title = request["title"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_set_pane_title(sn, pane_id, title)
                    .await
            }
            "find_pane_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let pattern = request["pattern"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_find_pane_text(sn, pane_id, pattern)
                    .await
            }
            "close_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_close_pane(sn, pane_id).await
            }
            "close_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_close_window(sn, idx).await
            }
            "kill_session" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_kill_session(sn).await
            }
            "rename_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let name = request["name"].as_str().unwrap_or("");
                protocol_proxy.handle_rename_window(sn, idx, name).await
            }
            "list_window_panes" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_list_window_panes(sn, idx).await
            }
            "resize_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let w = request["width"].as_u64().map(|v| v as u16);
                let h = request["height"].as_u64().map(|v| v as u16);
                protocol_proxy.handle_resize_window(sn, idx, w, h).await
            }
            "select_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_select_window(sn, idx).await
            }
            "select_layout" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let layout = request["layout"].as_str().unwrap_or("tiled");
                protocol_proxy.handle_select_layout(sn, idx, layout).await
            }
            "pane_info" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_pane_info(sn, pane_id).await
            }
            "window_info" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_window_info(sn, idx).await
            }
            "pane_exists" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_pane_exists(sn, pane_id).await
            }
            _ => json!({"error": format!("unknown request type: {}", req_type)}),
        };

        send_response(&writer, &response).await?;
    }

    Ok(())
}

async fn send_response(
    writer: &Arc<tokio::sync::Mutex<impl AsyncWriteExt + Unpin>>,
    response: &serde_json::Value,
) -> Result<()> {
    let resp_json = serde_json::to_string(response)?;
    let mut w = writer.lock().await;
    w.write_all(&(resp_json.len() as u32).to_le_bytes()).await?;
    w.write_all(resp_json.as_bytes()).await?;
    Ok(())
}

pub async fn proxy_legacy<S>(first_byte: u8, stream: S, protocol_proxy: &ProtocolProxy) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let prefixed = PrefixedStream { first: Some(first_byte), inner: stream };
    proxy_protocol_aware(prefixed, protocol_proxy).await
}

struct PrefixedStream<S> {
    first: Option<u8>,
    inner: S,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(b) = self.first.take() {
            buf.put_slice(&[b]);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
