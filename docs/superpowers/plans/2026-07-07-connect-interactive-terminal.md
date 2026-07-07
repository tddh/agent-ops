# agent-ops connect 交互式终端 Phase 1 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 `agent-ops connect` Phase 1：bridge 端 PTY 双向转发 + CLI crate 骨架

**Architecture:** Bridge 端新增 `interactive.rs` 处理 QUIC 0x06/0x07 流；CLI 端新增 `agent-ops-cli` crate；双方通过 QUIC bidi streams 共享 `Arc<Mutex<Option<InteractiveSession>>>` 状态

**Tech Stack:** Rust + quinn + rmux-sdk + crossterm + clap

**参考设计:** `docs/connect-design.md`

---

### Task 1: 新增 ProtocolProxy 公开方法

**Files:**
- Modify: `crates/rmux-bridge/src/protocol.rs`

ProtocolProxy 需要新增两个方法供 interactive.rs 调用。

- [ ] **Step 1: 添加 get_session 和 get_pane 方法**

在 `impl ProtocolProxy` 块末尾（`handle_find_text_all` 之后），添加：

```rust
    /// 获取 rmux session 对象（供 interactive 模块使用）
    pub async fn get_session(&self, name: &str) -> anyhow::Result<rmux_sdk::Session> {
        let sn = SessionName::new(name).map_err(|e| anyhow::anyhow!("{}", e))?;
        self.rmux.session(sn).await.map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// 获取 rmux pane 对象（供 interactive 模块使用）
    pub async fn get_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> anyhow::Result<rmux_sdk::Pane> {
        let pane_id = Self::parse_pane_id(pane_id_str)
            .ok_or_else(|| anyhow::anyhow!("invalid pane_id: {}", pane_id_str))?;
        let sn = SessionName::new(session_name)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        self.rmux
            .get_pane_by_id(&sn, pane_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
```

- [ ] **Step 2: 验证编译**

Run: `cargo check -p rmux-bridge`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/rmux-bridge/src/protocol.rs
git commit -m "feat(bridge): add get_session and get_pane public methods for interactive module"
```

---

### Task 2: 创建 bridge 端 interactive.rs

**Files:**
- Create: `crates/rmux-bridge/src/interactive.rs`
- Modify: `crates/rmux-bridge/src/main.rs` (line 2, add `mod interactive;`)

- [ ] **Step 1: 创建 interactive.rs 文件（交互式会话状态 + TLV 帧协议 + 控制流/数据流处理器）**

```rust
//! 交互式终端处理器：处理 QUIC 0x06（控制流）和 0x07（数据流）
//!
//! 参考：docs/connect-design.md

use anyhow::{Context, Result};
use quinn::{RecvStream, SendStream};
use rmux_sdk::{PaneOutputChunk, TerminalSizeSpec};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

use crate::protocol::ProtocolProxy;

/// 交互式会话状态（在控制流和数据流之间共享）
pub struct InteractiveSession {
    pub session_name: String,
    pub pane_id: String,
    pub cols: u16,
    pub rows: u16,
    /// 进程退出码（由数据流 0x07 写入，控制流 0x06 读取后发送 ProcessExited）
    pub exit_code: Option<i32>,
    /// 进程退出通知（数据流写入 exit_code 后唤醒控制流）
    pub exit_notify: Arc<Notify>,
}

/// Ghost Buffer 大小：~64KB，受 TLV u16 负载限制（65535 - 4 字节头部）
const SCROLLBACK_BUFFER_SIZE: usize = 65531;

// ─── QUIC stream 读取辅助 ───

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

// ─── 控制流处理 (0x06) ───

pub async fn handle_interactive_control(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<ProtocolProxy>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    // 1. 读取 Attach 消息
    let msg_type = read_u8(&mut recv).await?;
    if msg_type != 0x01 {
        write_error(&mut send, 0x03, "expected Attach message first").await?;
        return Ok(());
    }
    let payload_len = read_u16_le(&mut recv).await? as usize;
    let payload = read_bytes(&mut recv, payload_len).await?;

    let (session_name, pane_id, cols, rows, _term) = parse_attach_payload(&payload)?;

    // 2. 验证 session
    let _session = match proxy.get_session(&session_name).await {
        Ok(s) => s,
        Err(_) => {
            write_error(&mut send, 0x01, &format!("session not found: {}", session_name)).await?;
            return Ok(());
        }
    };

    // 3. 验证 pane
    let pane = match proxy.get_pane(&session_name, &pane_id).await {
        Ok(p) => p,
        Err(e) => {
            write_error(&mut send, 0x02, &format!("pane not found: {}", e)).await?;
            return Ok(());
        }
    };

    // 4. 获取 Ghost Buffer
    let snapshot = pane.snapshot().await?;
    let scrollback = snapshot.visible_text().into_bytes();
    let scrollback_len = scrollback.len().min(SCROLLBACK_BUFFER_SIZE);
    let scrollback = scrollback[scrollback.len().saturating_sub(scrollback_len)..].to_vec();

    // 5. 设置终端尺寸
    pane.resize(TerminalSizeSpec::new(cols, rows)).await?;

    // 6. 写入共享状态
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

    // 7. 发送 Attached 确认
    write_attached(&mut send, &scrollback).await?;

    // 8. 控制消息循环（同时监控进程退出）
    let exit_notify_ref = exit_notify;

    loop {
        let msg_result = tokio::select! {
            r = read_u8(&mut recv) => Some(r),
            _ = exit_notify_ref.notified() => None,
        };

        let msg_type = match msg_result {
            Some(Ok(t)) => t,
            Some(Err(_)) | None => {
                if let Some(exit_code) = session_state
                    .lock().await
                    .as_ref()
                    .and_then(|s| s.exit_code)
                {
                    write_process_exited(&mut send, exit_code).await?;
                    tracing::info!(
                        "process exited in {}/{}: code={}",
                        session_name, pane_id, exit_code
                    );
                }
                break;
            }
        };

        let payload_len = read_u16_le(&mut recv).await? as usize;
        let payload = read_bytes(&mut recv, payload_len).await?;

        match msg_type {
            0x02 => {
                // Resize
                let new_cols = u16::from_le_bytes([payload[0], payload[1]]);
                let new_rows = u16::from_le_bytes([payload[2], payload[3]]);
                pane.resize(TerminalSizeSpec::new(new_cols, new_rows)).await?;
                tracing::debug!("resize: {}x{}", new_cols, new_rows);
            }
            0x03 => {
                // Detach
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

// ─── 数据流处理 (0x07) ───

pub async fn handle_interactive_data(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<ProtocolProxy>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    // 等待 0x06 完成 Attach（最多 30 秒）
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
    let mut output_stream = pane.output_stream().await?;

    // 双向 relay
    let input_to_pty = {
        let pane = pane.clone();
        async {
            let mut buf = [0u8; 4096];
            loop {
                match recv.read(&mut buf).await? {
                    Some(n) => {
                        let keys = String::from_utf8_lossy(&buf[..n]);
                        pane.send_key(&keys).await?;
                    }
                    None => break,
                }
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    let pty_to_output = {
        let session_state = session_state.clone();
        async {
            use futures::StreamExt;
            while let Some(chunk) = output_stream.next().await {
                match chunk {
                    PaneOutputChunk::Bytes { bytes, .. } => {
                        send.write_all(&bytes).await?;
                    }
                    PaneOutputChunk::Lag(_) => continue,
                    _ => continue,
                }
            }
            // 进程退出
            let mut state = session_state.lock().await;
            if let Some(ref mut s) = *state {
                s.exit_code = Some(0);
                s.exit_notify.notify_one();
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    tokio::try_join!(input_to_pty, pty_to_output)?;
    Ok(())
}

// ─── 协议编解码 ───

fn parse_attach_payload(data: &[u8]) -> Result<(String, String, u16, u16, String)> {
    let mut offset = 0;

    let session_name_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;
    let session_name =
        String::from_utf8(data[offset..offset + session_name_len].to_vec())?;
    offset += session_name_len;

    let pane_id_len = data[offset] as usize;
    offset += 1;
    let pane_id =
        String::from_utf8(data[offset..offset + pane_id_len].to_vec())?;
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
```

- [ ] **Step 2: 在 main.rs 中注册模块**

在 `crates/rmux-bridge/src/main.rs` 第 2 行的 `mod config;` 之后添加：

```rust
mod interactive;
```

- [ ] **Step 3: 验证编译**

Run: `cargo check -p rmux-bridge`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/rmux-bridge/src/interactive.rs crates/rmux-bridge/src/main.rs
git commit -m "feat(bridge): add interactive.rs with 0x06/0x07 stream handlers"
```

---

### Task 3: 修改 files.rs 添加 0x06/0x07 分发

**Files:**
- Modify: `crates/rmux-bridge/src/files.rs` (lines 235-255, function `handle_quic_stream`)

- [ ] **Step 1: 修改 handle_quic_stream 函数签名和 match 分支**

将现有的 `handle_quic_stream` 函数的签名和 match 块修改为：

```rust
use crate::interactive::InteractiveSession;

pub async fn handle_quic_stream(
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    protocol_proxy: std::sync::Arc<crate::protocol::ProtocolProxy>,
    session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>>,
) -> anyhow::Result<()> {
    let mut type_buf = [0u8; 1];
    recv.read_exact(&mut type_buf).await?;
    match type_buf[0] {
        0x01 => {
            let adapter = crate::proxy::QuicStreamAdapter { recv, send };
            crate::proxy::proxy_protocol_aware(adapter, &protocol_proxy).await
        }
        0x02 => handle_upload_quic(send, recv).await,
        0x03 => handle_download_quic(send, recv).await,
        0x05 => handle_tunnel_quic(send, recv).await,
        0x06 => crate::interactive::handle_interactive_control(
            send, recv, protocol_proxy.clone(), session_state.clone(),
        ).await,
        0x07 => crate::interactive::handle_interactive_data(
            send, recv, protocol_proxy, session_state,
        ).await,
        t => {
            tracing::warn!("unknown QUIC stream type: 0x{:02x}", t);
            Ok(())
        }
    }
}
```

注意：需要在文件顶部添加 `use crate::interactive::InteractiveSession;`。查找现有的 `use` 块并添加。

- [ ] **Step 2: 验证编译**

Run: `cargo check -p rmux-bridge`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/rmux-bridge/src/files.rs
git commit -m "feat(bridge): add 0x06/0x07 stream dispatch in handle_quic_stream"
```

---

### Task 4: 修改 main.rs QUIC 连接处理器注入 session_state

**Files:**
- Modify: `crates/rmux-bridge/src/main.rs` (lines ~130-151, QUIC connection handler loop)

- [ ] **Step 1: 修改 QUIC 连接处理器，为每个连接创建 session_state**

将 `main.rs` 中 QUIC 连接处理器中的流处理循环（约第 126-152 行）：

```rust
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
```

修改为：

```rust
                let protocol_proxy = match ProtocolProxy::connect(&rmux_socket).await {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        tracing::error!("QUIC rmux connect failed: {}", e);
                        return;
                    }
                };

                // 每个 QUIC 连接一个 session_state，0x06/0x07 流共享
                let session_state: std::sync::Arc<
                    tokio::sync::Mutex<Option<interactive::InteractiveSession>>,
                > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

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
```

- [ ] **Step 2: 验证编译**

Run: `cargo check -p rmux-bridge`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/rmux-bridge/src/main.rs
git commit -m "feat(bridge): create session_state per QUIC connection for interactive streams"
```

---

### Task 5: 创建 CLI crate 骨架

**Files:**
- Create: `crates/agent-ops-cli/Cargo.toml`
- Create: `crates/agent-ops-cli/src/main.rs`
- Create: `crates/agent-ops-cli/src/connect.rs`
- Create: `crates/agent-ops-cli/src/terminal.rs`
- Create: `crates/agent-ops-cli/src/protocol.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "agent-ops-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "agent-ops"
path = "src/main.rs"

[dependencies]
agent-ops-core.workspace = true
anyhow.workspace = true
clap = { workspace = true, features = ["derive"] }
crossterm = { version = "0.28", features = ["event-stream"] }
quinn.workspace = true
rustls.workspace = true
serde.workspace = true
serde_yaml = "0.9"
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
futures.workspace = true
```

- [ ] **Step 2: 创建 main.rs（CLI 入口）**

```rust
//! agent-ops CLI: 人类运维命令行工具

mod connect;
mod protocol;
mod terminal;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-ops", about = "AI Agent 远程运维 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// 主机配置文件路径
    #[arg(long, default_value = "config/hosts.yaml")]
    hosts_file: String,

    /// CA 证书路径（用于 TLS 验证）
    #[arg(long, default_value = "config/ca.crt")]
    ca_cert: String,
}

#[derive(Subcommand)]
enum Commands {
    /// 交互式连接到远程 rmux 会话
    Connect {
        /// 目标主机名（在 hosts.yaml 中定义）
        host: String,

        /// rmux session 名称
        #[arg(long, default_value = "agent-ops")]
        session: String,

        /// pane ID
        #[arg(long, default_value = "%0")]
        pane: String,

        /// 只读模式（不发送输入）
        #[arg(long)]
        readonly: bool,
    },

    /// 列出可连接的会话和 pane
    List {
        /// 目标主机名
        host: String,
    },
}

fn load_host_config(hosts_file: &str, host_name: &str) -> anyhow::Result<agent_ops_core::HostConfig> {
    let contents = std::fs::read_to_string(hosts_file)?;
    let registry: agent_ops_core::HostRegistry = serde_yaml::from_str(&contents)?;
    registry
        .hosts
        .into_iter()
        .find(|h| h.name == host_name)
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found in {}", host_name, hosts_file))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect { host, session, pane, readonly } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            connect::connect(&config, &cli.ca_cert, &session, &pane, readonly).await
        }
        Commands::List { host } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            connect::list_sessions(&config, &cli.ca_cert).await
        }
    }
}
```

- [ ] **Step 3: 创建 protocol.rs（协议编解码占位）**

```rust
//! 控制流/数据流协议的客户端编解码

use anyhow::Result;
use quinn::SendStream;

/// 发送 Attach 请求
pub async fn write_attach_request(
    send: &mut SendStream,
    session_name: &str,
    pane_id: &str,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let term = "xterm-256color";
    // 计算 payload 长度
    let payload_len = 2 + session_name.len() + 1 + pane_id.len() + 2 + 2 + 1 + term.len();

    send.write_all(&[0x01]).await?; // message type: Attach
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&(session_name.len() as u16).to_le_bytes()).await?;
    send.write_all(session_name.as_bytes()).await?;
    send.write_all(&[pane_id.len() as u8]).await?;
    send.write_all(pane_id.as_bytes()).await?;
    send.write_all(&cols.to_le_bytes()).await?;
    send.write_all(&rows.to_le_bytes()).await?;
    send.write_all(&[term.len() as u8]).await?;
    send.write_all(term.as_bytes()).await?;
    Ok(())
}

/// 读取 Attached 响应（返回 scrollback 数据）
pub async fn read_attached_response(recv: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut type_buf = [0u8; 1];
    recv.read_exact(&mut type_buf).await?;
    if type_buf[0] == 0x82 {
        // Error response
        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf).await?;
        let payload_len = u16::from_le_bytes(len_buf) as usize;
        let mut payload = vec![0u8; payload_len];
        recv.read_exact(&mut payload).await?;
        let _code = payload[0];
        let msg_len = u16::from_le_bytes([payload[1], payload[2]]) as usize;
        let msg = String::from_utf8_lossy(&payload[3..3 + msg_len]);
        anyhow::bail!("bridge error: {}", msg);
    }
    if type_buf[0] != 0x81 {
        anyhow::bail!("unexpected response type: 0x{:02x}", type_buf[0]);
    }

    let mut len_buf = [0u8; 2];
    recv.read_exact(&mut len_buf).await?;
    let payload_len = u16::from_le_bytes(len_buf) as usize;

    let mut scrollback_len_buf = [0u8; 4];
    recv.read_exact(&mut scrollback_len_buf).await?;
    let scrollback_len = u32::from_le_bytes(scrollback_len_buf) as usize;

    let mut scrollback = vec![0u8; scrollback_len];
    recv.read_exact(&mut scrollback).await?;
    Ok(scrollback)
}

/// 发送 Resize 消息
pub async fn write_resize(
    send: &mut SendStream,
    cols: u16,
    rows: u16,
) -> Result<()> {
    send.write_all(&[0x02]).await?;
    send.write_all(&4u16.to_le_bytes()).await?;
    send.write_all(&cols.to_le_bytes()).await?;
    send.write_all(&rows.to_le_bytes()).await?;
    Ok(())
}

/// 发送 Detach 消息
pub async fn write_detach(send: &mut SendStream) -> Result<()> {
    send.write_all(&[0x03]).await?;
    send.write_all(&0u16.to_le_bytes()).await?;
    Ok(())
}

/// 发送 JSON 帧（用于 list_sessions）
pub async fn send_json_frame(send: &mut SendStream, value: &serde_json::Value) -> Result<()> {
    let json_str = serde_json::to_string(value)?;
    let len = json_str.len() as u32;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(json_str.as_bytes()).await?;
    Ok(())
}

/// 接收 JSON 帧（用于 list_sessions）
pub async fn recv_json_frame(recv: &mut quinn::RecvStream) -> Result<serde_json::Value> {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    let value: serde_json::Value = serde_json::from_slice(&buf)?;
    Ok(value)
}
```

- [ ] **Step 4: 创建 connect.rs（连接核心逻辑）**

```rust
//! 交互式连接核心逻辑

use agent_ops_core::HostConfig;
use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 建立到 bridge 的 QUIC 连接（TLS 1.3 + AUTH frame 认证）
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

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(tls_config)));

    let conn = endpoint
        .connect(bridge_addr.parse()?, "rmux-bridge")?
        .await?;

    // 认证
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

/// 交互式连接到远程 rmux 会话
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

    // 控制流
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    ctrl_send.write_all(&[0x06]).await?;
    crate::protocol::write_attach_request(
        &mut ctrl_send, session_name, pane_id, cols, rows,
    ).await?;

    let scrollback = crate::protocol::read_attached_response(&mut ctrl_recv).await?;

    // 数据流
    let (mut data_send, mut data_recv) = conn.open_bi().await?;
    data_send.write_all(&[0x07]).await?;

    enable_raw_mode()?;

    // 输出 scrollback
    if !scrollback.is_empty() {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&scrollback).await?;
        stdout.flush().await?;
    }

    let ctrl_send = std::sync::Arc::new(tokio::sync::Mutex::new(ctrl_send));

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
    crate::protocol::write_detach(&mut *ctrl_send.lock().await).await.ok();

    result
}

async fn stdin_to_quic(send: &mut quinn::SendStream) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf).await?;
        if n == 0 { break; }
        send.write_all(&buf[..n]).await?;
    }
    Ok(())
}

async fn quic_to_stdout(recv: &mut quinn::RecvStream) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        match recv.read(&mut buf).await? {
            Some(n) => {
                stdout.write_all(&buf[..n]).await?;
                stdout.flush().await?;
            }
            None => break,
        }
    }
    Ok(())
}

async fn resize_watcher(
    ctrl_send: std::sync::Arc<tokio::sync::Mutex<quinn::SendStream>>,
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

/// 列出远程主机上的会话和 pane
pub async fn list_sessions(config: &HostConfig, ca_cert_path: &str) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({ "type": "session_list" });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if let Some(sessions) = response.get("sessions").and_then(|s| s.as_array()) {
        println!("{:<20} {:<10} {}", "SESSION", "PANES", "CREATED");
        println!("{}", "-".repeat(50));
        for session in sessions {
            println!(
                "{:<20} {:<10} {}",
                session["session_name"].as_str().unwrap_or("-"),
                "-",
                "-",
            );
        }
    }

    Ok(())
}

use std::sync::Arc;
```

- [ ] **Step 5: 创建 terminal.rs（终端管理）**

```rust
//! 终端管理：raw mode 守卫

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// 终端守卫：确保退出时恢复终端状态
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter_raw_mode() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
```

- [ ] **Step 6: 更新 workspace Cargo.toml**

在 `/Users/tddh/code/agent-ops/Cargo.toml` 的 `members` 数组中添加：
```toml
    "crates/agent-ops-cli",
```

同时添加 workspace dependency：
```toml
serde_yaml = "0.9"
```

- [ ] **Step 7: 验证编译**

Run: `cargo check -p agent-ops-cli`
Expected: PASS

Run: `cargo check --workspace`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/agent-ops-cli/ Cargo.toml
git commit -m "feat(cli): add agent-ops-cli crate with connect and list commands"
```

---

### Task 6: 最终验证

**Files:** N/A (build only)

- [ ] **Step 1: 全量编译检查**

Run: `cargo check --workspace`
Expected: PASS

- [ ] **Step 2: Lint 检查**

Run: `cargo clippy --workspace -- -D warnings`
Expected: PASS (或仅有 pre-existing warnings)

- [ ] **Step 3: 测试**

Run: `cargo test --workspace`
Expected: PASS

---
