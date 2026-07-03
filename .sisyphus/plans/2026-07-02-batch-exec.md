# batch_exec / batch_upload / batch_download 多机并发操作方案

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `batch_exec`、`batch_upload`、`batch_download` 三个 MCP 工具，使 AI agent 能对多台主机并发执行命令、上传文件、下载文件，结果一次返回。

**Architecture:** `batch_exec` 在已有的 `exec` 函数基础上抽取 `exec_in_session` 通用 helper，通过 `tokio::spawn` + `Vec<JoinHandle>` 对每台主机并发执行。`batch_upload` / `batch_download` 直接复用 `files.rs` 中已有的 `upload_file` / `download_file`，同样 fan-out 并发。三个工具共享相同的并发控制模型（`concurrency` 参数 + `tokio::sync::Semaphore`）。per-host 错误隔离，单台主机故障不影响其他主机。共享 `agent-ops` session 的默认 pane `%0`。

**Tech Stack:** Rust + tokio (tokio::spawn, Semaphore), 复用现有 `connect_to_bridge_hybrid` / `session_create` / `send_json_frame` / `upload_file` / `download_file`

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `crates/agent-ops-core/src/types.rs` | 修改 | 新增 `AuditAction::BatchExec`、`BatchUpload`、`BatchDownload` |
| `crates/agent-ops-mcp/src/tools.rs` | 修改 | 新增 `exec_in_session` + `batch_exec`、`batch_upload`、`batch_download`，抽取现有 `exec` |
| `crates/agent-ops-mcp/src/main.rs` | 修改 | 注册 `batch_exec`、`batch_upload`、`batch_download` 工具定义 + 描述 |
| `docs/TOOLS.md` | 修改 | 添加 `batch_exec`、`batch_upload`、`batch_download` 文档 |

无新文件。`files.rs` 不需要修改 —— `batch_upload` / `batch_download` 直接复用已有 `upload_file` / `download_file`。

---

## 设计要点

### session / pane 策略

> 单 agent 不存在同主机并发，默认 `%0`，行为与现有 `exec` 一致。

```
对每台主机（并发）：
  session_attach("agent-ops")
    ├─ 存在 → 直接使用
    └─ 不存在 → session_create("agent-ops")
  exec_in_session(%0, command) → 结果
```

不需要 `split_pane` / `close_pane` —— agent 对这台主机的操作是顺序的，`batch_exec` 跑完前不会对同主机发下一个请求。

### 错误隔离

| 故障点 | 处理 |
|--------|------|
| 主机不在 registry | 返回 `{ok: false, error: "host not found"}`，不抛异常 |
| DNS / TCP / TLS 失败 | 同上 |
| session_create 失败 | 同上 |
| 命令执行超时 | 返回 partial output + `{ok: false, error: "timeout"}` |
| 命令 exit_code != 0 | **不标 error**，正常返回 exit_code（非零退出是命令结果，不是执行失败） |

### 并发模型

- `tokio::spawn` + `Vec<JoinHandle>` 管理 N 个独立 task，每台主机一个
- `concurrency` 参数控制最大同时连接数（`tokio::sync::Semaphore`），默认 5，0 = 不限制
- 每台主机 `tokio::time::timeout` 独立超时
- 主循环 `run_mcp_stdio_loop` 不变（仍然是串行读 stdin，`batch_exec` 内部并发）

### 不变

- 现有 `exec` 工具行为完全不变
- 现有 36 个工具全部不受影响
- 不新增依赖
- 不修改 bridge 协议
- `ToolContext` 结构体不变

---

## 任务拆解

### Task 1: 新增 `AuditAction::BatchExec` 枚举值

**Files:**
- Modify: `crates/agent-ops-core/src/types.rs:110`

- [ ] **Step 1: 添加枚举值**

在 `AuditAction` 枚举末尾（`CmdEscape` 之前或之后任意位置）加一行：

```rust
    BatchExec,
```

完整上下文（第 74-110 行末尾）：

```rust
pub enum AuditAction {
    SessionCreate,
    // ... 省略已有 ...
    CmdEscape,
    StreamSubscribe,
    BatchExec,      // ← 新增
}
```

- [ ] **Step 2: 编译验证**

```bash
cargo check -p agent-ops-core
```

Expected: success（adding a variant to a non-exhaustive enum compiles fine）

- [ ] **Step 3: Commit**

```bash
git add crates/agent-ops-core/src/types.rs
git commit -m "feat: add AuditAction::BatchExec for multi-host concurrent exec"
```

---

### Task 2: 抽取 `exec_in_session` helper + 新增 `ExecResult` struct

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs`

- [ ] **Step 1: 在 `tools.rs` 现有 `exec` 函数之前添加 `ExecResult` 结构体**

在 `pub async fn execute_tool` 之上，`use uuid::Uuid;` 之下，添加：

```rust
/// 单次命令执行的结果，用于 batch 聚合和单机 exec 复用
struct ExecResult {
    ok: bool,
    output: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    error: Option<String>,
}
```

- [ ] **Step 2: 新增 `exec_in_session` helper 函数**

在 `ExecResult` struct 之后、现有 `async fn exec` 之前插入：

```rust
/// 在已有 session + pane 中执行一次性命令并等待结果。
/// 抽取自 `exec` 函数，供 `exec` 和 `batch_exec` 复用。
/// 不负责建连、建 session、写 audit。
async fn exec_in_session<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    command: &str,
    timeout_ms: u64,
    max_lines: usize,
) -> ExecResult
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    let marker_id = Uuid::new_v4().to_string();
    let marker_id = &marker_id[..3];
    let start_marker = format!("[{}]", marker_id);
    let sentinel_marker = format!("[{} ", marker_id);

    let keys = format!("\x15echo '{s}'\n{c}\necho \"{e}$?]\"\n",
        s = start_marker, c = command, e = sentinel_marker);
    send_json_frame(stream, &json!({
        "type": "send_keys",
        "session_name": session_name,
        "pane_id": pane_id,
        "keys": keys,
    })).await;

    let send_resp = match recv_json_frame(stream).await {
        Ok(r) => r,
        Err(e) => return ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(format!("send_keys: {e}")),
        },
    };

    if !send_resp["ok"].as_bool().unwrap_or(false) {
        let err = send_resp["error"].as_str().unwrap_or("send_keys failed").to_string();
        return ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(err),
        };
    }

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let mut last_text = String::new();
    let mut poll_interval = std::time::Duration::from_millis(50);

    loop {
        if let Err(e) = send_json_frame(stream, &json!({
            "type": "capture_pane",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_lines": max_lines,
        })).await {
            return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("capture_pane: {e}")),
            };
        }
        let resp = match recv_json_frame(stream).await {
            Ok(r) => r,
            Err(e) => return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("recv: {e}")),
            },
        };
        last_text = resp["text"].as_str().unwrap_or("").to_string();

        if let Some(pos) = last_text.find(&sentinel_marker) {
            let after_sentinel = &last_text[pos + sentinel_marker.len()..];
            let exit_code: Option<i32> = after_sentinel
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok();

            let output_before_sentinel = &last_text[..pos];
            let current_output = if let Some(start_pos) = output_before_sentinel.rfind(&start_marker) {
                let after_start = start_pos + start_marker.len();
                &output_before_sentinel[after_start..]
            } else {
                &last_text[..pos]
            };

            use std::borrow::Cow;
            let output_lines: Vec<Cow<str>> = current_output
                .lines()
                .filter(|line| {
                    let t = line.trim();
                    if t == "clear" { return false; }
                    if t.starts_with(command) || t == command { return false; }
                    if t.starts_with("echo") && t.contains(&sentinel_marker) { return false; }
                    if t.starts_with('[') && t.len() >= 4
                        && t.as_bytes().get(1).map_or(false, |b| b.is_ascii_hexdigit())
                        && t.as_bytes().get(2).map_or(false, |b| b.is_ascii_hexdigit())
                        && t.as_bytes().get(3).map_or(false, |b| b.is_ascii_hexdigit())
                        && t.get(4..).map_or(false, |s| s == "]" || s.starts_with(" "))
                    {
                        return false;
                    }
                    true
                })
                .map(Cow::Borrowed)
                .collect();

            let output = output_lines.join("\n").trim().to_string();
            let duration_ms = start.elapsed().as_millis() as u64;

            return ExecResult {
                ok: exit_code == Some(0),
                output,
                exit_code,
                duration_ms,
                error: None,
            };
        }

        if std::time::Instant::now() >= deadline {
            let duration_ms = start.elapsed().as_millis() as u64;
            return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms,
                error: Some(format!("timeout waiting for sentinel after {}ms", timeout_ms)),
            };
        }

        tokio::time::sleep(poll_interval).await;
        poll_interval = std::cmp::min(poll_interval * 2, std::time::Duration::from_millis(500));
    }
}
```

- [ ] **Step 3: 改造现有 `exec` 函数，调用 `exec_in_session`**

替换 `tools.rs` 第 440-570 行的 `async fn exec` 为精简版本：

```rust
async fn exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let clear_screen = args["clear_screen"].as_bool().unwrap_or(false);

    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    // clear_screen 在 exec_in_session 之前单独处理
    if clear_screen {
        send_json_frame(&mut tls, &json!({
            "type": "send_keys", "session_name": session_name, "pane_id": pane_id,
            "keys": "clear\n",
        })).await?;
        let _ = recv_json_frame(&mut tls).await;
    }

    let result = exec_in_session(&mut tls, session_name, pane_id, command, timeout_ms, max_lines).await;

    let output_summary: String = result.output.chars().take(500).collect();
    audit(ctx, AuditAction::Exec, host_name, session_name, Some(pane_id), command,
        Some(&output_summary), result.ok, result.duration_ms, result.error.as_deref()).await;

    Ok(json!({
        "ok": result.ok,
        "output": result.output,
        "exit_code": result.exit_code,
        "duration_ms": result.duration_ms,
        "error": result.error,
    }))
}
```

注意：删除 `tools.rs` 中原来 `exec` 函数末尾的 `send_json_frame` + `recv_json_frame` helper（如果它们不再被其他地方引用）。但这两个 helper 还在 `capture_pane`、`session_create` 等函数中使用，所以保留不动。

需要在文件顶部确认 `use std::borrow::Cow;` 是否已存在。如果不存在，在 `use` 区域添加。

- [ ] **Step 4: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: success

- [ ] **Step 5: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "refactor(exec): extract exec_in_session helper, reuse for batch_exec"
```

---

### Task 3: 实现 `batch_exec` 函数

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs`

- [ ] **Step 1: 在 `tools.rs` 中添加 `batch_exec` async fn**

位置：放在 `async fn exec` 之后（`resize_pane` 之前或附近）。

先确认文件顶部 use 区域有 `use tokio_stream::StreamExt;` 或 `use futures::StreamExt;`。检查 `Cargo.toml` 是否已有 `tokio-stream` 依赖 — 如果没有，用 `std::pin::pin` + `while let` 模式代替，不需要额外依赖。

实际上 `tools.rs` 没有显式依赖 `futures`。`FuturesUnordered` 来自 `futures` crate，需要确认是否已在依赖中。

先检查依赖：

```bash
grep -r "futures" crates/agent-ops-mcp/Cargo.toml
```

如果没有，改用纯 tokio 方案 — 用 `tokio::task::JoinSet` 或者手动 `Vec<JoinHandle>` + `for handle in handles`。不必为这一个场景引入 `futures` 依赖。

用 `tokio::task::JoinSet` 替代 `FuturesUnordered`（tokio 1.38+ 支持，条件编译或直接用 `JoinHandle`）。

实际上更简单的方式：用 `Vec<tokio::task::JoinHandle<(String, ExecResult)>>` + `for handle in handles`。不需要额外依赖。

```rust
async fn batch_exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({ "ok": true, "command": "", "total": 0, "success": 0, "failed": 0, "error": "empty hosts list" }));
    }

    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(120000);
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    // 解析主机名 → 验证存在，不存在的提前标 error
    let mut targets: Vec<(String, Option<agent_ops_core::types::HostConfig>)> = Vec::new();
    for name in &hosts_arg {
        match ctx.router.get(name) {
            Ok(h) => targets.push((name.clone(), Some(h.clone()))),
            Err(_) => targets.push((name.clone(), None)),
        }
    }

    let semaphore: Option<Arc<tokio::sync::Semaphore>> = if concurrency_limit > 0 {
        Some(Arc::new(tokio::sync::Semaphore::new(concurrency_limit)))
    } else { None };

    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let cmd = command.to_string();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, ExecResult)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let cmd = cmd.clone();
        let sem = semaphore.clone();

        let handle = tokio::spawn(async move {
            // concurrency limit
            let _permit = if let Some(s) = &sem {
                let p = s.acquire().await;
                p.ok()
            } else { None };

            let host = match host_opt {
                Some(h) => h,
                None => return (host_name, ExecResult {
                    ok: false,
                    output: String::new(),
                    exit_code: None,
                    duration_ms: 0,
                    error: Some("host not found in registry".into()),
                }),
            };

            // 1. connect
            let mut stream = match connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                ca_cert.as_deref(), 3, insecure,
            ).await {
                Ok(s) => s,
                Err(e) => return (host_name.clone(), ExecResult {
                    ok: false,
                    output: String::new(),
                    exit_code: None,
                    duration_ms: 0,
                    error: Some(format!("connect: {e}")),
                }),
            };

            let session_name = "agent-ops";

            // 2. ensure session exists
            let exists = match session_attach_inner(&mut stream, session_name).await {
                Ok(resp) => resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                Err(_) => false,
            };
            if !exists {
                if let Err(e) = create_session_inner(&mut stream, session_name).await {
                    return (host_name, ExecResult {
                        ok: false,
                        output: String::new(),
                        exit_code: None,
                        duration_ms: 0,
                        error: Some(format!("session_create: {e}")),
                    });
                }
            }

            // 3. exec on default pane %0
            let result = exec_in_session(&mut stream, session_name, "%0", &cmd, timeout_ms, max_lines).await;

            (host_name, result)
        });

        handles.push(handle);
    }

    // collect results
    let mut results_map = serde_json::Map::new();
    let mut success_count = 0u32;
    let mut failed_count = 0u32;

    for handle in handles {
        match handle.await {
            Ok((host_name, result)) => {
                if result.ok && result.error.is_none() {
                    success_count += 1;
                } else {
                    failed_count += 1;
                }
                results_map.insert(host_name, json!({
                    "ok": result.ok && result.error.is_none(),
                    "output": result.output,
                    "exit_code": result.exit_code,
                    "duration_ms": result.duration_ms,
                    "error": result.error,
                }));
            }
            Err(e) => {
                failed_count += 1;
                results_map.insert("unknown".into(), json!({
                    "ok": false, "output": "", "exit_code": null,
                    "duration_ms": 0, "error": format!("task cancelled: {e}"),
                }));
            }
        }
    }

    let total_duration_ms = start.elapsed().as_millis() as u64;

    audit(ctx, AuditAction::BatchExec, "", "", None,
        &format!("hosts:{:?} cmd:{}", hosts_arg, cmd), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0,
        "command": command,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}
```

- [ ] **Step 2: 新增两个 inner helper 函数**

`batch_exec` 依赖以下两个低开销 helper（在同一个文件 `tools.rs` 中，放在 `batch_exec` 之前）：

```rust
/// 内部 session_attach（不记 audit，直接返回 bridge 响应）
async fn session_attach_inner(stream: &mut BridgeStream, session_name: &str) -> Result<Value> {
    send_json_frame(stream, &json!({ "type": "attach_session", "session_name": session_name })).await?;
    recv_json_frame(stream).await
}

/// 内部 session_create（不记 audit，直接返回 bridge 响应）
async fn create_session_inner(stream: &mut BridgeStream, session_name: &str) -> Result<Value> {
    send_json_frame(stream, &json!({ "type": "new_session", "name": session_name, "detached": true })).await?;
    recv_json_frame(stream).await
}
```

注意：`BridgeStream` 需要被 tools.rs 引用。检查文件顶部是否已有 `use crate::transport::BridgeStream;`。如果没有，添加。

- [ ] **Step 3: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: success。解决任何编译错误（通常是 missing import、type mismatch）。

- [ ] **Step 4: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "feat: add batch_exec for multi-host concurrent command execution"
```

---

### Task 4: 注册 `batch_exec` 到 MCP 工具列表

**Files:**
- Modify: `crates/agent-ops-mcp/src/main.rs`

- [ ] **Step 1: 在 `tools_definition` JSON 中添加工具定义**

在 `main.rs` 的 `tools_definition` 中，`pane_exists` 定义之后（但在 `]` 闭合之前）添加：

```json
,
            {
                "name": "batch_exec",
                "description": "Multi-host command execution: sends the same command to all specified hosts concurrently, waits for each to complete (sentinel polling), captures output per host, and returns results keyed by hostname. Default 5 concurrent connections, 200 lines/host, 2min timeout/host. Host-level failures (connection refused, timeout) are marked ok=false but do NOT affect other hosts. Non-zero exit codes are NOT treated as errors — they are part of the command result. For self-terminating commands only (ls, cat, grep, df, systemctl, kubectl, curl). NOT for interactive programs (vim, htop) or non-terminating commands (tail -f, ping). Uses the agent-ops session default pane (%0) on each host. Use this when you need to run the same command on multiple machines in one round — saves N-1 round trips compared to calling exec per host.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list, max 64" },
                        "command": { "type": "string", "description": "Command to execute on each host" },
                        "timeout_ms": { "type": "number", "description": "Per-host timeout in ms (default: 120000)" },
                        "max_lines": { "type": "integer", "description": "Max output lines per host (default: 200, 0=unlimited)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "command"]
                }
            }
```

- [ ] **Step 2: 在 `execute_tool` match 中添加分支**

在 `tools.rs` 的 `execute_tool` match 中（第 60 行附近），`_ => anyhow::bail!(...)` 之前添加：

```rust
        "batch_exec" => batch_exec(ctx, args).await,
```

- [ ] **Step 3: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: success

- [ ] **Step 4: Commit**

```bash
git add crates/agent-ops-mcp/src/main.rs
git commit -m "feat: register batch_exec tool in MCP definitions"
```

---

### Task 5: 更新文档

**Files:**
- Modify: `docs/TOOLS.md`

- [ ] **Step 1: 在 TOOLS.md 末尾添加 `batch_exec` 文档**

在 `docs/TOOLS.md` 末尾，`### \`pane_exists\`` 段落下新增：

````markdown
---

## 批量操作

### `batch_exec`

Execute the same command on multiple hosts concurrently. Sends the command to all specified hosts in parallel, waits for each to complete via sentinel polling, captures output per host, and returns results keyed by hostname. Host-level failures (connection refused, timeout) do not affect other hosts. Non-zero exit codes are NOT errors — they are part of the command result. For self-terminating commands only.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表，最多 64 |
| `command` | string | ✅ | 要在每台主机上执行的命令 |
| `timeout_ms` | number | | 每台主机超时毫秒数（默认 120000） |
| `max_lines` | integer | | 每台主机最大返回行数（默认 200，0=不限制） |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "command": "uptime",
  "total": 3,
  "success": 2,
  "failed": 1,
  "total_duration_ms": 2345,
  "results": {
    "tf01": {
      "ok": true,
      "output": "12:34:56 up 30 days ...",
      "exit_code": 0,
      "duration_ms": 1234
    },
    "tf02": {
      "ok": false,
      "output": "",
      "exit_code": null,
      "duration_ms": 5000,
      "error": "connect: connection refused"
    }
  }
}
```

- `results` 以主机名为 key，直接取值不遍历
- 单台主机故障（连接失败/超时/命令错误）不抛异常，在对应 result 中标记 `ok: false` + `error`
- `total_duration_ms` 是墙钟时间（所有主机中最长的那台），反映并发效果
- 非零 exit_code **不** 标记为 error——exit_code 是命令结果，不是执行失败
- 内部通过 `agent-ops` session 的默认 pane `%0` 执行，行为与 `exec` 一致
````

- [ ] **Step 2: 编译验证**

文档改动，不需要编译。

- [ ] **Step 3: Commit**

```bash
git add docs/TOOLS.md
git commit -m "docs: add batch_exec tool documentation"
```

---

### Task 6: 端到端验证

- [ ] **Step 1: 编译项目**

```bash
cargo build -p agent-ops-mcp --release
```

Expected: success

- [ ] **Step 2: 部署 bridge 到 tf01（如需）**

确认 `config/hosts.test.yaml` 中有可用的主机配置。

- [ ] **Step 3: 单主机测试 — 确保不破坏现有 `exec`**

在已运行的 agent-ops session 中执行：

```
exec host=tf01 session_name=agent-ops pane_id=%0 command="hostname"
```

Expected: 正常返回 hostname，行为与之前一致。

- [ ] **Step 4: 单主机 `batch_exec` 测试**

```
batch_exec hosts=["tf01"] command="id -u"
```

Expected:

```json
{
  "ok": true,
  "total": 1,
  "success": 1,
  "results": {
    "tf01": { "ok": true, "exit_code": 0, "output": "0" }
  }
}
```

- [ ] **Step 5: 无效 host 测试**

```
batch_exec hosts=["tf01", "no-such-host"] command="uptime"
```

Expected: tf01 正常返回，`no-such-host` 返回 `{ok: false, error: "host not found in registry"}`，整体 `failed: 1`。

- [ ] **Step 6: 命令超时测试**

```
batch_exec hosts=["tf01"] command="sleep 10" timeout_ms=2000
```

Expected: tf01 返回 `{ok: false, error: "timeout waiting for sentinel after 2000ms"}`。

- [ ] **Step 7: 多主机并发测试**（需要至少 2 台可用主机）

```
batch_exec hosts=["tf01","tf02"] command="sleep 3 && hostname"
```

Expected: 总耗时约 3s（不是 6s），两台主机各自返回 hostname。

---

### Task 8: 实现 `batch_upload` + `batch_download`

**Files:**
- Modify: `crates/agent-ops-core/src/types.rs`
- Modify: `crates/agent-ops-mcp/src/tools.rs`
- Modify: `crates/agent-ops-mcp/src/main.rs`
- Modify: `docs/TOOLS.md`

> 文件传输的 fan-out 模式与 `batch_exec` 相同。`upload_file` / `download_file` 已封装完整单主机逻辑，`batch_*` 只负责并发 + 聚合。`files.rs` 本身不需要修改。

**前置：在 Task 3（`batch_exec`）实现时，将以下三段逻辑抽取为共享 helper。** 三个 batch 工具的核心模式完全一致 —— 只是内层调用的函数不同。

```rust
/// 解析主机名列表 → (name, Option<HostConfig>)
fn resolve_hosts(ctx: &ToolContext, names: &[String]) -> Vec<(String, Option<agent_ops_core::types::HostConfig>)> {
    names.iter().map(|name| {
        match ctx.router.get(name) {
            Ok(h) => (name.clone(), Some(h.clone())),
            Err(_) => (name.clone(), None),
        }
    }).collect()
}

/// 创建并发信号量（concurrency=0 → None，即不限制）
fn make_semaphore(limit: usize) -> Option<Arc<tokio::sync::Semaphore>> {
    if limit > 0 { Some(Arc::new(tokio::sync::Semaphore::new(limit))) } else { None }
}

/// 收集 JoinHandle 结果 → (results_map, success_count, failed_count)
async fn collect_batch_results(
    handles: Vec<tokio::task::JoinHandle<(String, Value)>>,
) -> (serde_json::Map<String, Value>, u32, u32) {
    let mut results_map = serde_json::Map::new();
    let mut success = 0u32;
    let mut failed = 0u32;
    for handle in handles {
        if let Ok((host_name, result)) = handle.await {
            if result["ok"].as_bool().unwrap_or(false) { success += 1; } else { failed += 1; }
            results_map.insert(host_name, result);
        } else {
            failed += 1;
            results_map.insert("unknown".into(), json!({"ok": false, "error": "task cancelled"}));
        }
    }
    (results_map, success, failed)
}
```

- [ ] **Step 0: 抽取共享 helper**（如 Task 3 未抽取，此处做一次重构）

- [ ] **Step 1: 添加 `AuditAction::BatchUpload` 和 `BatchDownload`**

在 `types.rs` 的 `AuditAction` 枚举中 `BatchExec` 之后：

```rust
    BatchUpload,
    BatchDownload,
```

- [ ] **Step 2: 在 `tools.rs` 中添加 `batch_upload` 函数**

```rust
async fn batch_upload(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array().context("missing 'hosts'")?
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "error": "empty hosts list", "total_duration_ms": 0, "results": {}}));
    }

    let local_path = args["local_path"].as_str().context("missing 'local_path'")?;
    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let overwrite = match args["overwrite"].as_str().unwrap_or("overwrite") {
        "skip" => crate::files::OverwriteMode::Skip,
        "rename" => crate::files::OverwriteMode::Rename,
        "error" => crate::files::OverwriteMode::NoClobber,
        _ => crate::files::OverwriteMode::Overwrite,
    };
    let exclude: Vec<String> = args["exclude"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let mut targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let local = local_path.to_string();
        let remote = remote_path.to_string();
        let exclude = exclude.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };
            let host = match host_opt {
                Some(h) => h,
                None => return (host_name, json!({"ok": false, "error": "host not found"})),
            };
            match crate::files::upload_file(&host, &local, &remote, ca_cert.as_deref(), insecure, overwrite, &exclude).await {
                Ok(files) => {
                    let uploaded = files.iter().filter(|f| f.status == "uploaded").count();
                    let failed = files.iter().filter(|f| f.status == "failed").count();
                    (host_name, json!({
                        "ok": failed == 0,
                        "files": files, "total": files.len(),
                        "uploaded": uploaded, "skipped": files.len() - uploaded - failed,
                        "failed_count": failed,
                    }))
                }
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    audit(ctx, AuditAction::BatchUpload, "", "", None,
        &format!("hosts:{:?} local:{}", hosts_arg, local_path), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}
```

- [ ] **Step 3: 在 `tools.rs` 中添加 `batch_download` 函数**

```rust
async fn batch_download(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array().context("missing 'hosts'")?
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "error": "empty hosts list", "total_duration_ms": 0, "results": {}}));
    }

    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let local_dir = args["local_dir"].as_str().context("missing 'local_dir'")?;
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let file_name = std::path::Path::new(remote_path)
        .file_name().map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| remote_path.to_string());

    let mut targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let remote = remote_path.to_string();
        let local_dir = local_dir.to_string();
        let file_name = file_name.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };
            let host = match host_opt {
                Some(h) => h,
                None => return (host_name.clone(), json!({"ok": false, "error": "host not found"})),
            };
            let local_path = format!("{}/{}/{}", local_dir, host_name, file_name);
            if let Some(parent) = std::path::Path::new(&local_path).parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return (host_name.clone(), json!({"ok": false, "error": format!("mkdir: {e}")}));
                }
            }
            match crate::files::download_file(&host, &remote, &local_path, ca_cert.as_deref(), insecure).await {
                Ok(file) => (host_name, json!({
                    "ok": true,
                    "file": {"remote_path": remote, "local_path": local_path,
                              "size": file.size, "sha256": file.sha256}
                })),
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    audit(ctx, AuditAction::BatchDownload, "", "", None,
        &format!("hosts:{:?} remote:{}", hosts_arg, remote_path), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}
```

> `resolve_hosts`、`make_semaphore`、`collect_batch_results` 已在本 Task 前置步骤定义。`batch_exec` 的 Task 3 实现时应同步抽取这三个 helper，此处直接复用。

- [ ] **Step 4: 在 `execute_tool` match 中添加分支 + 注册工具定义**

`tools.rs`:

```rust
        "batch_upload" => batch_upload(ctx, args).await,
        "batch_download" => batch_download(ctx, args).await,
```

`main.rs` 工具定义（紧接 batch_exec 之后）：

```json
,
            {
                "name": "batch_upload",
                "description": "Upload a file or directory to multiple hosts concurrently. Each host receives the same file(s) at the specified remote_path. Per-host error isolation. Supports overwrite modes (overwrite|skip|rename|error) and exclude glob patterns. Default 5 concurrent connections. Uses QUIC transport.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list" },
                        "local_path": { "type": "string", "description": "Local file or directory path" },
                        "remote_path": { "type": "string", "description": "Remote destination path" },
                        "overwrite": { "type": "string", "description": "overwrite|skip|rename|error (default: overwrite)" },
                        "exclude": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns to exclude (directories only)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "local_path", "remote_path"]
                }
            },
            {
                "name": "batch_download",
                "description": "Download a file from multiple hosts concurrently. Saves to local_dir/<hostname>/<filename> to avoid host-to-host overwrites. ⚠️ Multiple runs to same local_dir WILL overwrite previous downloads. Use different local_dir per run to preserve history. Returns per-host size and SHA256. Per-host error isolation. Default 5 concurrent connections. Uses QUIC transport.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list" },
                        "remote_path": { "type": "string", "description": "Remote file path to download" },
                        "local_dir": { "type": "string", "description": "Local directory (files saved as <local_dir>/<hostname>/<filename>)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "remote_path", "local_dir"]
                }
            }
```

- [ ] **Step 5: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

- [ ] **Step 6: 更新 TOOLS.md**

在 `docs/TOOLS.md` 的 `### \`batch_exec\`` 段落下新增：

````markdown
### `batch_upload`

Upload a file or directory to multiple hosts concurrently.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表 |
| `local_path` | string | ✅ | 本地文件或目录路径 |
| `remote_path` | string | ✅ | 远程目标路径 |
| `overwrite` | string | | overwrite\|skip\|rename\|error（默认 overwrite） |
| `exclude` | string[] | | 排除 glob 模式（仅目录上传） |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "total": 3,
  "success": 2,
  "failed": 1,
  "total_duration_ms": 5678,
  "results": {
    "tf01": {
      "ok": true,
      "files": [{"path": "/opt/app", "status": "uploaded", "size": 12345, "sha256": "abc..."}],
      "total": 1, "uploaded": 1, "skipped": 0, "failed_count": 0
    },
    "tf02": {
      "ok": false,
      "error": "connect: connection refused"
    }
  }
}
```

### `batch_download`

Download a file from multiple hosts concurrently. Each host's file is saved to `local_dir/<hostname>/<filename>`. ⚠️ Multiple runs to the same `local_dir` WILL overwrite previous downloads — use different `local_dir` per run to preserve history.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表 |
| `remote_path` | string | ✅ | 远程文件路径 |
| `local_dir` | string | ✅ | 本地目录（自动创建 `<hostname>/` 子目录） |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "total": 2,
  "success": 1,
  "failed": 1,
  "total_duration_ms": 3456,
  "results": {
    "tf01": {
      "ok": true,
      "file": {
        "remote_path": "/var/log/syslog",
        "local_path": "./logs/tf01/syslog",
        "size": 987654,
        "sha256": "def..."
      }
    },
    "tf02": {
      "ok": false,
      "error": "download failed: file not found"
    }
  }
}
```
````

- 多次 download 到同一 `local_dir` 会覆盖已有文件。如需保留历史版本，agent 应每次指定不同的 `local_dir`。

- [ ] **Step 7: Commit**

```bash
git add crates/agent-ops-core/src/types.rs crates/agent-ops-mcp/src/tools.rs crates/agent-ops-mcp/src/main.rs docs/TOOLS.md
git commit -m "feat: add batch_upload and batch_download for multi-host file transfer"
```

---

### Task 9: 文件传输端到端验证

- [ ] **Step 1: 单文件批量上传**

```
batch_upload hosts=["tf01","tf02"] local_path="/tmp/test.txt" remote_path="/tmp/test.txt"
```

Expected: 两台各返回 `{"ok": true, "uploaded": 1}`。

- [ ] **Step 2: 错误隔离测试**

```
batch_upload hosts=["tf01","no-such-host"] local_path="/tmp/test.txt" remote_path="/tmp/test.txt"
```

Expected: tf01 正常，`no-such-host` 返回 `{"ok": false, "error": "host not found"}`。

- [ ] **Step 3: 批量下载**

```
batch_download hosts=["tf01","tf02"] remote_path="/etc/hostname" local_dir="/tmp/batch-dl"
```

Expected: 本地生成 `/tmp/batch-dl/tf01/hostname`、`/tmp/batch-dl/tf02/hostname`，含对应主机名。

---

### Task 10: 清理 & 验证

- [ ] **Step 1: 运行完整检查**

```bash
just check
just lint
```

Expected: 无错误。

- [ ] **Step 2: 运行测试**

```bash
cargo test --workspace
```

Expected: 全部通过。

- [ ] **Step 3: Review 改动范围**

```bash
git log --oneline -9
git diff main..HEAD --stat
```

Expected: ~6 个 commit，只改了 `types.rs` / `tools.rs` / `main.rs` / `TOOLS.md`。

---

## 自审清单

- [x] **Spec coverage**: `batch_exec` → Task 4；`batch_upload` → Task 8 Step 2；`batch_download` → Task 8 Step 3；并发控制 → Semaphore + `concurrency` 参数（三个工具统一）；session 复用 → `agent-ops` + 默认 `%0`（batch_exec）；错误隔离 → per-host catch-all
- [x] **Placeholder scan**: 所有步骤有具体代码或命令，无 TBD/TODO
- [x] **Type consistency**: `ExecResult` 在 Task 2 定义，Task 2/3 使用。`AuditAction` 三个 variant 在 Task 1 + Task 8 定义，各 audit 调用使用。`OverwriteMode` 在 `files.rs` 已有，`batch_upload` 直接引用。
