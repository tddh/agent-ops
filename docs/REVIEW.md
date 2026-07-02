# agent-ops 全面审查报告

> 审查日期：2026-07-02 | 范围：代码功能 + 文档完整性 + 安全健壮性

---

## 🔴 P0 — 必须修复

### 1. `host_filter` 工具缺少审计调用

**位置**：`crates/agent-ops-mcp/src/tools.rs:79-106`

**问题**：`host_filter` 执行过滤后直接返回，是 33 个 MCP 工具中唯一完全缺失审计的（除 stream_pane 外）。

**修复方案**：

1. 在 `crates/agent-ops-core/src/types.rs` 的 `AuditAction` 枚举中添加 `HostFilter` 变体：
```rust
pub enum AuditAction {
    // ... existing variants ...
    HostFilter,       // ← 新增
}
```

2. 在 `crates/agent-ops-mcp/src/tools.rs` 的 `host_filter` 函数 `Ok(json!(...))` 返回前添加：
```rust
audit(ctx, AuditAction::HostFilter, "", "", None, "",
    Some(&format!("group={:?} tags={:?} pattern={:?}",
        args.get("group"), args.get("tags"), args.get("pattern"))),
    true, 0, None).await;
Ok(json!({ "hosts": result, "count": result.len() }))
```

---

### 2. `stream_pane` 工具缺少审计调用

**位置**：`crates/agent-ops-mcp/src/tools.rs:293-302`

**问题**：`stream_pane` 直接返回 `recv_json_frame` 结果，无审计。

**修复方案**：

1. 在 `types.rs` 的 `AuditAction` 枚举中添加 `StreamSubscribe` 变体。

2. 将 `tools.rs` 的 `stream_pane` 函数改为：
```rust
async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("agent-ops");
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut conn = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token,
        ctx.ca_cert_path.as_deref(), 3, false).await?;
    let req = json!({"type":"stream_subscribe","session_name":session_name,"pane_id":pane_id});
    send_json_frame(&mut conn, &req).await?;
    let response = recv_json_frame(&mut conn).await?;
    audit(ctx, AuditAction::StreamSubscribe, host_name, session_name, Some(pane_id),
        "", None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}
```

---

### 3. `split_pane` 使用错误的 `AuditAction` 变体

**位置**：`crates/agent-ops-mcp/src/tools.rs:584`，`crates/agent-ops-core/src/types.rs`

**问题**：分割 pane 却记录为 `SplitWindow`。`AuditAction` 枚举中不存在 `SplitPane`。

**修复方案**：

1. 在 `types.rs` 的 `AuditAction` 枚举中添加：
```rust
pub enum AuditAction {
    // ... existing ...
    SplitPane,      // ← 新增
    SplitWindow,
}
```

2. 在 `tools.rs:584` 修改：
```rust
// Before:
audit(ctx, AuditAction::SplitWindow, host_name, ...)
// After:
audit(ctx, AuditAction::SplitPane, host_name, ...)
```

---

### 4. QUIC 流分发碰撞 Bug

**位置**：`crates/rmux-bridge/src/files.rs:251-253`，`crates/agent-ops-mcp/src/transport.rs`

**问题**：bridge 端 `handle_quic_stream` 读取第一个字节判断流类型。当 JSON 帧长度 `%256 == 2或3` 时，LE32 第一字节与文件流魔数（0x02/0x03）冲突，导致 JSON 请求被错误路由到文件处理器。概率约 1/128。

**修复方案**：

在 JSON 协议流前插入一个专用魔数字节 `0x01`（JSON 协议标识），避免与 LE32 长度前缀冲突：

1. 修改 `crates/agent-ops-mcp/src/transport.rs` 的 `connect_to_bridge_hybrid` 中 QUIC 路径，新开 JSON bidi stream 后先写入魔数：
```rust
// After opening json bidi stream:
let (json_send, json_recv) = conn.open_bi().await?;
json_send.write_all(&[0x01]).await?;  // ← JSON 协议魔数
return Ok(BridgeStream::Quic { conn, send: json_send, recv: json_recv });
```

2. 修改 `crates/rmux-bridge/src/files.rs` 的 `handle_quic_stream` 增加 `0x01` 分发：
```rust
match type_buf[0] {
    0x01 => {  // ← JSON 协议流
        let adapter = QuicStreamAdapter { recv, send };
        proxy_protocol_aware(adapter, &protocol_proxy).await
    }
    0x02 => handle_upload_quic(send, recv).await,
    0x03 => handle_download_quic(send, recv).await,
    _ => { tracing::warn!("unknown stream type: 0x{:02x}", t); Ok(()) }
}
```

3. 移除 `protocol_proxy: Option<...>` 参数，改为必传（因为 JSON 流始终需要 proxy），简化代码。

---

### 5. `--insecure` 标志文档存在但 CLI 代码缺失

**位置**：`crates/agent-ops-mcp/src/main.rs:17-35`，`crates/agent-ops-mcp/src/transport.rs`

**问题**：READM/DEPLOY 均文档化了 `--insecure`，但 CLI 未实现。`transport.rs` 有 `SkipVerification` 逻辑但不可达。

**修复方案**：

1. 在 `main.rs` 的 `Cli` 结构体中添加：
```rust
struct Cli {
    // ... existing ...
    #[arg(long)]
    insecure: bool,
}
```

2. 在 `main.rs` 的 `ToolContext` 初始化时传入：
```rust
let ctx = ToolContext {
    router: Arc::new(router),
    ca_cert_path: cli.ca_cert.clone(),
    insecure: cli.insecure,  // ← 新增
    // ...
};
```

3. 在 `tools.rs` 的 `ToolContext` 中添加字段：
```rust
pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub insecure: bool,  // ← 新增
    // ...
}
```

4. 将所有 `connect_to_bridge_hybrid(..., false)` 改为：
```rust
connect_to_bridge_hybrid(..., ctx.insecure)
```

5. 同步更新 `config/mcp-config.example.json`。

---

### 6. README 中 `--ca-cert` 示例路径错误

**位置**：`README.md:99`，`README.zh.md:100`

**问题**：MCP 配置示例用 `bridge.crt` 作为 `--ca-cert`。远程部署模式下应使用 `ca.crt`。

**修复方案**：

在 README 中修改 MCP 配置示例，区分两种场景：

```json
{
  "mcp": {
    "agent-ops": {
      "type": "local",
      "command": ["/path/to/agent-ops-mcp"],
      "args": [
        "--hosts-file", "/path/to/hosts.yaml",
        "--ca-cert", "/path/to/ca.crt"
      ],
      "enabled": true
    }
  }
}
```
并添加注释：*远程部署使用 `ca.crt`；本地自签名测试可用 `bridge.crt`*。

---

### 7. MCP 工具计数不一致

**位置**：`README.md:137`，`README.zh.md:138`

**问题**：文档说 35 个工具，代码有 36 个（含 `stream_pane`）。

**修复方案**：

READM 中改为："35 个可用工具（+ 1 个开发中 `stream_pane`）"，并在工具表的 Output 分类中补充一行 `stream_pane`（标注 ⚠️ 当前不可用）。

---

## 🟠 P1 — 应当修复

### 8. `session_create` 默认 session_name 不一致

**位置**：`docs/TOOLS.md:50`，`crates/agent-ops-mcp/src/tools.rs:110`

**问题**：代码默认 `"agent-ops"`，TOOLS.md 声称默认 `"agent-session"`。

**修复方案**：修改 TOOLS.md 第 50 行：
```
| `session_name` | string | | 会话名称（可选，默认 `agent-ops`） |
```

---

### 9. `upload_dir` 并发失败时结果被丢弃

**位置**：`crates/agent-ops-mcp/src/files.rs:174-181`

**问题**：失败的上传仅 `tracing::warn!` 后跳过，不进入 results。`total`/`failed` 计数不准确。

**修复方案**：
```rust
// Before:
match h.await? {
    Ok(r) => results.push(r),
    Err(e) => tracing::warn!("upload failed: {}", e),
}

// After:
match h.await? {
    Ok(r) => results.push(r),
    Err(e) => {
        tracing::warn!("upload failed: {}", e);
        results.push(FileResult {
            status: "failed".to_string(),
            path: String::new(),
            size: 0,
            sha256: None,
            error: Some(e.to_string()),
        });
    }
}
```

---

### 10. `audit stats --since` 参数未使用

**位置**：`crates/agent-ops-mcp/src/audit/query.rs:127`

**问题**：`_since` 参数接收但从未传入 SQL 查询。

**修复方案**：
```rust
// Before:
pub async fn stats(&self, _since: Option<String>) -> Result<String> {
    let conn = self.conn.lock().unwrap();
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))?;
    // ...

// After:
pub async fn stats(&self, since: Option<String>) -> Result<String> {
    let conn = self.conn.lock().unwrap();
    let (total, since_clause) = if let Some(ref s) = since {
        (conn.query_row("SELECT COUNT(*) FROM audit_log WHERE timestamp >= ?1", [s], |r| r.get(0))?, s.clone())
    } else {
        (conn.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))?, String::new())
    };
    // ... 在后续查询中复用 since_clause
```

---

### 11. `exec` 输出过滤过于激进

**位置**：`crates/agent-ops-mcp/src/tools.rs:484-486`

**问题**：所有 `[X]` 格式（≤8 字符）行被误删，包括合法输出如 `[OK]`、`[WARN]`。

**修复方案**：将过滤条件从宽泛的 `[`...`]` 改为精确匹配 3 字符 UUID 前缀格式：
```rust
// Before:
if t.starts_with('[') && t.ends_with(']') && t.len() <= 8 {
    return false;
}

// After:
// 只过滤 sentinel marker 行（UUID 3 字符前缀）
if t.starts_with('[') && t.len() >= 4 && t.as_bytes().get(1).map_or(false, |b| b.is_ascii_hexdigit())
    && t.as_bytes().get(2).map_or(false, |b| b.is_ascii_hexdigit())
    && t.as_bytes().get(3).map_or(false, |b| b.is_ascii_hexdigit())
    && t.get(4..).map_or(false, |s| s == "]" || s.starts_with(" ")) {
    return false;
}
```

---

### 12. `exec` 轮询间隔固定 200ms

**位置**：`crates/agent-ops-mcp/src/tools.rs:518`

**问题**：快速命令多等 200ms，高频轮询浪费带宽。

**修复方案**：使用指数退避，初始 50ms，最大 500ms：
```rust
// Before:
tokio::time::sleep(Duration::from_millis(200)).await;

// After:
let mut poll_interval = Duration::from_millis(50);
// ... in loop:
tokio::time::sleep(poll_interval).await;
poll_interval = std::cmp::min(poll_interval * 2, Duration::from_millis(500));
```

---

### 13. CHANGELOG.md 严重过时

**位置**：`CHANGELOG.md`

**问题**：仅记录初始版本，缺失 QUIC、CA 证书、Windows 支持、连接限制等。

**修复方案**：补充完整的 v0.1.0 条目：
```markdown
## [0.1.0] — 2026-07-02

### Added
- 36 MCP 工具（35 可用 + 1 开发中）
- QUIC 优先 + TCP/TLS 自动降级双协议传输
- CA 签发 + 按主机独立证书的多主机 PKI 体系
- Windows/macOS/Linux 客户端原生支持
- Bridge 并发连接限制（`--max-connections`，默认 256）
- Token 认证 + JWT 支持，恒定时间比较
- SQLite 审计日志（query/stats/cleanup）
- 主机注册表（group/tag/label 过滤、broadcast_keys）
- 文件传输：QUIC 上传/下载、目录递归并发上传
- systemd 服务部署 + `just deploy` 一键部署

### Fixed
- 生产代码 `unwrap()` 改为 poison-safe 模式
- Bridge QUIC handler 支持 JSON RPC 终端操作
```

---

### 14. `OverwriteMode::Rename` 和 `NoClobber` 未在 Bridge 端实现

**位置**：`crates/rmux-bridge/src/files.rs:handle_upload_quic`

**问题**：Bridge 只处理 Skip(0x02)，Rename(0x03) 和 NoClobber(0x04) 行为等同于 Overwrite。

**修复方案**：
```rust
// After reading mode byte:
match mode {
    0x02 => { // Skip
        if Path::new(&remote_path).exists() {
            send.write_all(&[0x01]).await?;  // skipped
            send.write_all(&0u64.to_le_bytes()).await?;
            send.write_all(&[0u8; 32]).await?;
            return Ok(());
        }
        // fall through to overwrite
    }
    0x03 => { // Rename
        let mut renamed = remote_path.clone();
        let mut counter = 1;
        while Path::new(&renamed).exists() {
            renamed = format!("{}.{}", remote_path, counter);
            counter += 1;
        }
        remote_path = renamed;
    }
    0x04 => { // NoClobber
        if Path::new(&remote_path).exists() {
            send.write_all(&[0x02]).await?;  // error: exists
            let msg = "file already exists";
            send.write_all(&(msg.len() as u16).to_le_bytes()).await?;
            send.write_all(msg.as_bytes()).await?;
            return Ok(());
        }
    }
    _ => {} // Overwrite
}
```

---

## 🟡 P2 — 建议修复

### 15. `drain_file_data` 死代码

**位置**：`crates/rmux-bridge/src/files.rs:231`

**修复方案**：删除函数及对应的 `#[allow(dead_code)]` 注解。

---

### 16. MCP 错误处理模式不一致

**位置**：`crates/agent-ops-mcp/src/tools.rs` 多处

**问题**：操作失败返回 `Ok(json!({"ok": false, ...}))`，AI 客户端在 JSON-RPC 层收到成功响应。

**修复方案**：对于可恢复的操作失败（如 session 已存在、pane 未找到），保持当前 `{"ok": false}` 模式；对于不可恢复的致命错误（如连接失败、认证失败），改为返回 `Err(...)`：
```rust
// 在 main.rs execute_tool 外层：
match tools::execute_tool(&ctx, tool_name, args).await {
    Ok(result) => {
        let is_error = result.get("ok").and_then(|v| v.as_bool()).map(|ok| !ok).unwrap_or(false);
        if is_error {
            json_rpc_response_with_error(id, &result)
        } else {
            json_rpc_response(id, &result)
        }
    }
    Err(e) => json_rpc_error(id, -32000, &e.to_string()),
}
```

---

### 17. 无连接池复用 ⏸️ 暂不实施

**状态**：当前短连接模式简单可靠，每连接独立状态无副作用。QUIC 0-RTT 已极大降低重连成本。**暂不实施**。

---

### 18. `MAX_FRAME_SIZE` 重复定义

**位置**：`crates/agent-ops-mcp/src/tools.rs:384`，`crates/rmux-bridge/src/proxy.rs:17`

**修复方案**：

1. 在 `crates/agent-ops-core/src/lib.rs` 中导出：
```rust
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;
```

2. 删除 `tools.rs` 和 `proxy.rs` 中的本地定义，改为 `use agent_ops_core::MAX_FRAME_SIZE;`。

---

### 19. 数值类型截断无溢出检查

**位置**：`crates/agent-ops-mcp/src/tools.rs:526, 641-642`

**问题**：`cols`/`rows`/`width`/`height` 使用 `as u16` 转换，>65535 时静默截断。

**修复方案**：
```rust
// Before:
let cols = args["cols"].as_u64().unwrap_or(80) as u16;

// After:
let cols = u16::try_from(args["cols"].as_u64().unwrap_or(80))
    .context("cols must be 0-65535")?;
```

---

### 20. `collect_files` 递归无深度限制

**位置**：`crates/agent-ops-mcp/src/files.rs:249`

**修复方案**：添加深度参数（默认 64），将递归改为栈安全的迭代：
```rust
async fn collect_files(base: &Path, dir: &Path, remote_base: &str,
    exclude: &[glob::Pattern], files: &mut Vec<FileEntry>, depth: u32) -> Result<()>
{
    if depth > 64 {
        return Err(anyhow!("directory too deep: {}", dir.display()));
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if exclude.iter().any(|p| p.matches_path(&path)) { continue; }
        if path.is_dir() {
            Box::pin(collect_files(base, &path, remote_base, exclude, files, depth + 1)).await?;
        } else {
            // ... file handling
        }
    }
    Ok(())
}
```

---

### 21. `stream_subscribe` Bridge 端 task 泄漏

**位置**：`crates/rmux-bridge/src/proxy.rs:221-267`

**问题**：客户端断开后，bridge 的 streaming task 持续写入失败。

**修复方案**：在 writer 写入循环中添加连接断开检测：
```rust
// In the stream forwarding loop:
loop {
    match timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(frame)) => {
            if writer.write_all(&frame).await.is_err() {
                break;  // connection closed, exit task
            }
        }
        Ok(None) => break,
        Err(_) => continue,
    }
}
```

---

### 22. DEPLOY.md Unix Socket 路径说明矛盾

**位置**：`docs/DEPLOY.md:262, 267`

**修复方案**：统一为：
```
rmux daemon 按运行用户 UID 创建 Unix socket：
- root → `/tmp/rmux-0/default`
- 普通用户（UID 1000） → `/tmp/rmux-1000/default`
部署脚本自动检测实际路径，无需手动指定。
```

---

### 23. `--max-connections` 未在 DEPLOY.md 中列出

**位置**：`docs/DEPLOY.md:121-131`

**修复方案**：在 Bridge CLI 参数表添加：
```
| `--max-connections` | `256` | 最大并发连接数，0=无限制（`MAX_CONNECTIONS` 环境变量） |
```

---

### 24. `CONTRIBUTING.md` GitHub URL 含占位符

**位置**：`CONTRIBUTING.md:12`

**修复方案**：将 `<org>` 替换为实际值：
```
git clone git@github.com:tddh/agent-ops.git
```

---

### 25. Bridge TCP 文件传输协议无人使用

**位置**：`crates/rmux-bridge/src/proxy.rs:62-218`，`crates/rmux-bridge/src/files.rs:8-114`

**问题**：两套 TCP 文件传输协议（proxy inline 二进制流 + yamux 流），MCP 客户端仅使用 QUIC。

**修复方案**：在 TCP handler 的主循环中注释或移除 `file_upload`/`file_download` JSON 路由（保留 QUIC 路径作为唯一文件传输方式）。如未来需要 TCP 文件传输后备，再恢复。

具体：在 `proxy.rs` 的 `proxy_protocol_aware` match 臂中为 `"file_upload"` 和 `"file_download"` 添加 `"use QUIC"` 的错误响应。

---

### 26. `EnsureSessionPolicy::CreateOrReuse` 状态污染风险

**位置**：`crates/rmux-bridge/src/protocol.rs:80`

**问题**：session 已存在时复用，可能带有残留 pane 状态。

**修复方案**：默认行为保持不变（兼容性），但为 `session_create` 添加 `force_new` 参数：
```rust
// 在 MCP tool add:
let force_new = args.get("force_new").and_then(|v| v.as_bool()).unwrap_or(false);
let policy = if force_new {
    EnsureSessionPolicy::CreateOnly
} else {
    EnsureSessionPolicy::CreateOrReuse
};
```
并在 TOOLS.md 中记录此行为。

---

## 🟢 P3 — 已跟踪

| # | 问题 | 修复方案 |
|---|------|---------|
| 27 | README.zh.md JSON 示例含中文注释（非法 JSON） | 将注释移到 JSON 代码块外部 |
| 28 | DEPLOY.md cert 流程说明冗余 | 合并步骤 2-3，标注 `just deploy` 自动处理证书 |
| 29 | `just certs-host` 等命令未记录 | 在 DEPLOY.md 补充 `certs-host` 和 `run-bridge` 说明 |
| 30 | `mcp-config.example.json` 缺审计参数 | 添加 `--audit-db`、`--audit-retention-days` 等示例 |
| 31 | systemd 模板未显式设 QUIC/连接数参数 | 在 `install-bridge.sh` 的 systemd ExecStart 中添加 `--quic-listen-addr 0.0.0.0:9778 --max-connections 256` |
| 32 | 健康检查端口 `+1` 可能溢出 | 改为 `port.checked_add(1).unwrap_or(9779)` |
| 33 | `cmd_escape` 发送冗余 `host` 字段 | 从 JSON payload 中移除 `host` 字段（bridge 不读取） |
| 34 | batch upload handler (0x04) 为隐藏功能 | 与 #25 一同评估，决定保留或移除 |
| 35 | `audit output_summary` UTF-8 边界 panic | 改为 `output.chars().take(500).collect::<String>()` |

---

## ✅ 已验证正确

以下各项经过交叉验证确认无误：

| 验证项 | 结果 |
|--------|:---:|
| 31 个 JSON type 字段 client ↔ bridge 匹配 | ✅ |
| 所有 JSON 参数名 client ↔ bridge 一致 | ✅ |
| 返回值字段全部匹配 | ✅ |
| 认证帧格式（TCP + QUIC）正确 | ✅ |
| exec marker/sentinel 机制正常 | ✅ |
| QUIC 文件传输二进制协议匹配 | ✅ |
| `hosts.yaml` 格式正确，token 不通过 `host_list` 泄露 | ✅ |
| Bridge `proxy.rs` 覆盖所有 33 个协议类型 | ✅ |
| `auth.rs` 使用恒定时间比较 | ✅ |
| `unsafe` 代码：0 处 | ✅ |

---

## 统计

| 严重程度 | 数量 |
|----------|:---:|
| 🔴 P0 必须修复 | 7 |
| 🟠 P1 应当修复 | 7 |
| 🟡 P2 建议修复 | 12 |
| 🟢 P3 已跟踪 | 9 |
| **总计** | **35** |
