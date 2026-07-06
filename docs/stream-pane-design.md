# stream_pane 重构设计方案

## 一、问题分析

### 当前实现的问题

`tools.rs:391 stream_pane()` 每次调用都通过 `connect_to_bridge_hybrid()` 新建 TLS 连接。连接在函数返回时关闭，后续调用无法复用，导致每次返回的都是完整快照，无法实现增量读取。

### 为什么会这样

```rust
// tools.rs - 当前 stream_pane 实现
async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    // ...参数解析...
    let mut tls = connect_to_bridge_hybrid(...).await?;  // ① 新建 TLS 连接
    send_json_frame(&mut tls, &json!({"type": "stream_subscribe", ...})).await?;
    let response = recv_json_frame(&mut tls).await?;       // ② 读一次响应
    Ok(response)                                           // ③ 连接关闭，后续调用从头开始
}
```

每次 MCP 调用都是独立请求-响应周期，连接无法跨调用保持。

## 二、参考设计

### tunnel.rs 的模式

```rust
// tunnel.rs - TunnelManager 在 MCP 端管理长连接
pub struct TunnelManager {
    tunnels: Arc<Mutex<HashMap<String, Tunnel>>>,
}

struct Tunnel {
    pub listener_task: JoinHandle<()>,  // 后台任务，保持连接活跃
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.listener_task.abort();     // Drop 时自动清理
    }
}
```

关键设计：
1. **状态在 MCP 端** — `TunnelManager` 保存所有活跃 tunnel 的连接和后台任务
2. **长连接** — QUIC/TLS 连接在 `create()` 时建立，由 `listener_task` 持有
3. **自动清理** — `Drop` trait 确保资源释放

### mcp-tmux 的 stream 模式

```
tmux -C attach -t <session>          ← 持久控制模式连接
  ├── %output %0 "line 1"            ← 输出事件
  ├── %output %0 "line 2"
  └── %exit                          ← 控制客户端退出

mcp-tmux 的处理：
  stream_start  → 创建 tmux -C 子进程，启动 _read_loop 后台任务
  stream_read   → 从 event buffer 读取新事件 (seq > cursor)
  stream_stop   → detach 控制客户端
```

关键点：连接在服务端保持，客户端通过 ID 引用。

## 三、目标方案

### 核心思路

在 MCP 端维护一个 `StreamManager`，管理所有活跃的 stream。`stream_pane` 首次调用时建立长连接，后续调用复用同一连接。

```
第 1 次 stream_pane(host, session, pane_id, timeout=3s):
  stream_manager 中没有此连接的记录
  → connect_to_bridge_hybrid() 建立 TLS 长连接
  → 发送 stream_subscribe（bridge 创建 output_stream）
  → 启动后台读取任务，持续从 bridge 读取数据到 buffer
  → 等待 buffer 有数据或超时
  → 返回第一块数据（包含快照）

第 2 次 stream_pane(host, session, pane_id, timeout=3s):
  stream_manager 中已有此连接
  → 直接复用已有的 buffer
  → 等待新数据或超时
  → 返回增量数据
```

### 不需要 stream_start/stream_stop

一个 `stream_pane` 工具就够了。首次调用了建立连接，后续调用复用。
连接通过超时自动清理（`reader_task` 检测到连接断开时自动从 map 移除）。

## 四、详细实现

### 4.1 新建文件：`crates/agent-ops-mcp/src/stream.rs`

```rust
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::transport::{
    connect_to_bridge_hybrid, BridgeStream, send_json_frame, recv_json_frame,
};
use agent_ops_core::types::HostConfig;

/// 管理所有活跃的 stream 连接
pub struct StreamManager {
    streams: Arc<Mutex<HashMap<String, Arc<StreamConnection>>>>,
}

/// 单个 stream 连接
struct StreamConnection {
    buffer: Arc<Mutex<VecDeque<String>>>,
    reader_task: JoinHandle<()>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// stream_pane 的核心逻辑：首次调用建连接，后续复用
    pub async fn stream_pane(
        &self,
        host: &HostConfig,
        session_name: &str,
        pane_id: &str,
        timeout_ms: u64,
        ca_cert_path: Option<&str>,
        insecure: bool,
    ) -> Result<Value> {
        // 用 host.name + session_name + pane_id 作为连接标识
        let key = format!("{}:{}:{}", host.name, session_name, pane_id);

        // ① 检查是否已有连接
        let conn = {
            let streams = self.streams.lock().await;
            streams.get(&key).cloned()
        };

        let conn = if let Some(c) = conn {
            // 已有连接，直接复用
            c
        } else {
            // ② 首次调用：建立长连接
            let mut tls = connect_to_bridge_hybrid(
                &host.bridge_addr,
                &host.bridge_token,
                ca_cert_path,
                3,
                insecure,
            )
            .await
            .with_context(|| format!("failed to connect to bridge for stream: {}", host.name))?;

            // ③ 发送 stream_subscribe 请求（bridge 端创建 output_stream）
            send_json_frame(
                &mut tls,
                &json!({
                    "type": "stream_subscribe",
                    "session_name": session_name,
                    "pane_id": pane_id,
                }),
            )
            .await
            .with_context(|| "failed to send stream_subscribe")?;

            // ④ 读取 bridge 的确认响应
            let _resp = recv_json_frame(&mut tls)
                .await
                .with_context(|| "failed to receive stream_subscribe ack")?;

            // ⑤ 创建共享 buffer 和连接对象
            let tls = Arc::new(Mutex::new(tls));
            let buffer = Arc::new(Mutex::new(VecDeque::new()));

            // ⑥ 启动后台读取任务：持续从 bridge 读数据，写入 buffer
            let tls_clone = tls.clone();
            let buffer_clone = buffer.clone();
            let key_clone = key.clone();
            let streams_clone = self.streams.clone();
            let reader_task = tokio::spawn(async move {
                loop {
                    let mut tls_guard = tls_clone.lock().await;
                    match recv_json_frame(&mut *tls_guard).await {
                        Ok(frame) => {
                            if let Some(text) = frame.get("text").and_then(|v| v.as_str()) {
                                let mut buf = buffer_clone.lock().await;
                                buf.push_back(text.to_string());
                            }
                        }
                        Err(_) => {
                            // 连接断开，从 map 中移除自己
                            streams_clone.lock().await.remove(&key_clone);
                            break;
                        }
                    }
                }
            });

            // ⑦ 注册到 map
            let conn = Arc::new(StreamConnection {
                tls: Arc::downgrade(&tls),  // 不需要强引用，仅标记连接存在
                _tls: tls,                   // 保持连接活跃
                buffer,
                reader_task,
            });

            let mut streams = self.streams.lock().await;
            streams.insert(key.clone(), conn.clone());
            conn
        };

        // ⑧ 从 buffer 读取数据（阻塞等待，直到有数据或超时）
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let text = {
                let mut buf = conn.buffer.lock().await;
                buf.pop_front()
            };

            if let Some(text) = text {
                return Ok(json!({"text": text}));
            }

            if std::time::Instant::now() >= deadline {
                return Ok(json!({"text": ""}));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
```

**问题**：上述设计中有 `Arc::downgrade` 和 `_tls` 字段，存在冗余。而且 `reader_task` 在结构体中但没有被外部使用。

### 4.2 修正后的实现

**关键发现**：Bridge 端的 `stream_subscribe` 使用**二进制帧协议**推送数据，不是 JSON 帧。

Bridge 端响应流程：
1. **Ack 响应**（标准 JSON 帧）：`4 字节 LE 长度 + JSON {"ok":true,"stream_subscribed":true,...}`
2. **数据帧**（二进制协议）：`0x02` + `pane_id_len(u32 LE)` + `pane_id` + `text_len(u32 LE)` + `text`

因此 reader_task 必须先读 ack 确认成功，再循环解析二进制数据帧。

```rust
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::transport::{connect_to_bridge_hybrid, send_json_frame, recv_json_frame};
use agent_ops_core::types::HostConfig;

/// 二进制 stream 帧的魔数
const STREAM_FRAME_MAGIC: u8 = 0x02;

/// buffer 最大容量，防止内存无限增长
const MAX_BUFFER_SIZE: usize = 10000;

pub struct StreamManager {
    streams: Arc<Mutex<HashMap<String, Arc<StreamConnection>>>>,
}

struct StreamConnection {
    buffer: Arc<Mutex<VecDeque<String>>>,
    /// 保存 JoinHandle 以支持 abort 清理
    reader_task: JoinHandle<()>,
}

impl Drop for StreamConnection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// 解析 bridge 端推送的二进制 stream 数据帧：0x02 + pid_len + pid + text_len + text
/// 返回 text 内容（忽略 pane_id，因为 subscribe 时已指定）
async fn read_stream_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> anyhow::Result<String> {
    // 1. 读魔数字节
    let mut magic = [0u8; 1];
    stream.read_exact(&mut magic).await?;
    if magic[0] != STREAM_FRAME_MAGIC {
        anyhow::bail!("unexpected stream frame magic: 0x{:02x}", magic[0]);
    }

    // 2. 读 pane_id_len + pane_id，跳过（subscribe 时已绑定 pane）
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let pid_len = u32::from_le_bytes(len_buf) as usize;
    if pid_len > 1024 {
        anyhow::bail!("pane_id too long: {} bytes", pid_len);
    }
    let mut pid = vec![0u8; pid_len];
    stream.read_exact(&mut pid).await?;

    // 3. 读 text_len + text
    stream.read_exact(&mut len_buf).await?;
    let text_len = u32::from_le_bytes(len_buf) as usize;
    if text_len > 1024 * 1024 {
        anyhow::bail!("stream text too large: {} bytes", text_len);
    }
    let mut text_buf = vec![0u8; text_len];
    stream.read_exact(&mut text_buf).await?;
    Ok(String::from_utf8_lossy(&text_buf).into_owned())
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn stream_pane(
        &self,
        host: &HostConfig,
        session_name: &str,
        pane_id: &str,
        timeout_ms: u64,
        ca_cert_path: Option<&str>,
        insecure: bool,
    ) -> anyhow::Result<serde_json::Value> {
        let key = format!("{}:{}:{}", host.name, session_name, pane_id);

        let conn = {
            let streams = self.streams.lock().await;
            streams.get(&key).cloned()
        };

        let conn = if let Some(c) = conn {
            c
        } else {
            let mut tls = connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                ca_cert_path, 3, insecure,
            ).await
            .with_context(|| format!("failed to connect to bridge for stream: {}", host.name))?;

            send_json_frame(&mut tls, &serde_json::json!({
                "type": "stream_subscribe",
                "session_name": session_name,
                "pane_id": pane_id,
            })).await
            .with_context(|| "failed to send stream_subscribe")?;

            // 检查 ack 响应是否成功
            let ack = recv_json_frame(&mut tls).await
                .with_context(|| "failed to receive stream_subscribe ack")?;
            if !ack.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                let err = ack.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
                anyhow::bail!("stream_subscribe failed: {}", err);
            }

            let tls = Arc::new(Mutex::new(tls));
            let buffer = Arc::new(Mutex::new(VecDeque::<String>::new()));

            let tls_clone = Arc::clone(&tls);
            let buffer_clone = Arc::clone(&buffer);
            let key_clone = key.clone();
            let streams_clone = Arc::clone(&self.streams);

            let reader_task = tokio::spawn(async move {
                loop {
                    let text = {
                        let mut guard = tls_clone.lock().await;
                        read_stream_frame(&mut *guard).await
                    };
                    match text {
                        Ok(t) => {
                            let mut buf = buffer_clone.lock().await;
                            // 防止 buffer 无限增长
                            if buf.len() < MAX_BUFFER_SIZE {
                                buf.push_back(t);
                            } else {
                                tracing::warn!(
                                    "stream buffer full ({MAX_BUFFER_SIZE}), dropping data for {key_clone}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::debug!("stream reader for {key_clone} disconnected: {e}");
                            streams_clone.lock().await.remove(&key_clone);
                            break;
                        }
                    }
                }
            });

            let conn = Arc::new(StreamConnection {
                buffer,
                reader_task,
            });

            self.streams.lock().await.insert(key.clone(), Arc::clone(&conn));
            conn
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(text) = conn.buffer.lock().await.pop_front() {
                return Ok(serde_json::json!({"text": text}));
            }
            if Instant::now() >= deadline {
                return Ok(serde_json::json!({"text": ""}));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
```

### 4.3 修改 `tools.rs` — stream_pane 函数

> **注意**：需要在文件头部添加 `use crate::stream::StreamManager;`

```rust
async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name  = args["host"].as_str().context("missing 'host'")?;
    let session    = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id    = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000);
    let host       = ctx.router.get(host_name)
                     .with_context(|| format!("host not found: {}", host_name))?;

    let start = std::time::Instant::now();
    let response = ctx.stream_manager.stream_pane(
        host, session, pane_id, timeout_ms,
        ctx.ca_cert_path.as_deref(), ctx.insecure,
    ).await?;
    let elapsed = start.elapsed().as_millis() as u64;

    let has_data = response["text"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
    audit(ctx, AuditAction::StreamSubscribe, host_name, session, Some(pane_id),
        "", None, has_data, elapsed, None).await;

    Ok(response)
}
```

### 4.4 修改 `tools.rs` — ToolContext

```rust
pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub insecure: bool,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: std::sync::Mutex<String>,
    pub tunnel_manager: Arc<TunnelManager>,
    pub stream_manager: Arc<StreamManager>,  // ← 新增
}
```

### 4.5 修改 `main.rs` — 初始化 StreamManager

```rust
let ctx = Arc::new(tools::ToolContext {
    router,
    ca_cert_path: cli.ca_cert,
    insecure: cli.insecure,
    audit_db,
    agent_name: std::sync::Mutex::new("unknown".to_string()),
    tunnel_manager: Arc::new(tunnel::TunnelManager::new()),
    stream_manager: Arc::new(stream::StreamManager::new()),  // ← 新增
});
```

### 4.6 修改 `main.rs` — 注册 stream 模块

```rust
mod stream;
```

### 4.7 Bridge 端 — 无需修改

`proxy.rs` 的 `stream_subscribe` 处理器流程：
1. 返回 ack JSON 帧确认订阅成功
2. spawn 后台 task，循环从 `output_stream.rx` 读取 pane 输出
3. 通过**二进制帧**持续推送给 MCP 端：`0x02` + `pane_id_len(u32 LE)` + `pane_id` + `text_len(u32 LE)` + `text`

MCP 端的 `reader_task` 需按此二进制协议解析数据帧（见 4.2 的 `read_stream_frame`）。

## 五、数据流

```
AI 端                      MCP 端 (agent-ops-mcp)           Bridge 端 (rmux-bridge)
┌────┐                     ┌──────────────────────┐          ┌─────────────────────┐
│ AI │──stream_pane()─────►│ StreamManager        │          │ ProtocolProxy       │
│    │                     │                      │          │                     │
│    │                     │ streams.get(key)     │          │                     │
│    │                     │   ├─ miss → 建连接    │          │                     │
│    │                     │   │   connect()──────┼─────────►│ authenticate()      │
│    │                     │   │   subscribe──────┼─────────►│ subscribe_pane()────┼──► output_stream
│    │                     │   │   check ack◄─────┼──────────│ {ok: true}          │   │
│    │                     │   │   spawn reader───┼──────┐   │   ┌─────────────────┤
│    │                     │   │       ↓          │      │   │   │ reader loop     │
│    │                     │   └─ hit  → 复用     │      │   │   │  rx.recv() ←───┼──► text chunks
│    │                     │                      │      └───┼───┤  push: 0x02+bin  │
│    │                     │  buffer.pop_front()  │◄─────────┼───│  read_stream_frame│
│    │◄──{text: "..."}─────│                      │          │   │                 │
│    │                     └──────────────────────┘          └─────────────────────┘
```

## 六、改动清单

| 文件 | 操作 | 内容 |
|------|------|------|
| `crates/agent-ops-mcp/src/stream.rs` | 新建 | StreamManager + stream_pane 方法 |
| `crates/agent-ops-mcp/src/tools.rs` | 修改 | stream_pane 改为调用 ctx.stream_manager.stream_pane()；导入 StreamManager |
| `crates/agent-ops-mcp/src/tools.rs` | 修改 | ToolContext 新增 stream_manager 字段 |
| `crates/agent-ops-mcp/src/main.rs` | 修改 | 添加 `mod stream`；初始化 StreamManager 到 ToolContext |
| `crates/agent-ops-mcp/src/transport.rs` | 无需修改 | send_json_frame / recv_json_frame 已添加 |
| `crates/rmux-bridge/` | 无需修改 | stream_subscribe 处理逻辑保持不变 |

## 七、与 mcp-tmux 的对比

| | mcp-tmux | agent-ops (本方案) |
|---|---|---|
| 连接管理 | ControlManager + HashMap | StreamManager + HashMap |
| 连接标识 | stream_id (UUID) | `host:session:pane` (隐式) |
| 工具数量 | 6 个 (start/read/send/resize/list/stop) | 1 个 (stream_pane) |
| 数据缓冲 | deque<Event> + cursor | VecDeque<String> (消费即删除) |
| 连接复用 | 显式 start，通过 stream_id 复用 | 隐式，首次调用建连接，后续自动复用 |
| 清理机制 | 显式 stream_stop + 自动重连 | Drop 时 tokio::spawn 自动 dropped (无需显式 stop) |
