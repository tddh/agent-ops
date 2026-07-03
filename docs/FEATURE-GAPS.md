# 待对接 rmux 功能设计文档

> 状态：已实现 | 最后更新：2026-07-04  
> 基于 rmux-sdk 0.7.1 API 与 agent-ops 现有封装对比分析

---

## 已知风险与待验证项

在实现前需优先验证以下关键假设：

| # | 风险 | 影响功能 | 验证方式 |
|---|------|----------|----------|
| R1 | rmux daemon 是否支持 `clear-history`、`list-buffers`、`paste-buffer`、`delete-buffer`、`break-pane`、`join-pane`、`swap-pane` CLI 命令 | 5, 6, 10, 11, 12 | 在目标 rmux 环境执行各命令 |
| R2 | `list-buffers` CLI 输出格式（纯文本），需要设计 Bridge 端解析方案 | 6a | 执行 `rmux list-buffers` 获取原始输出 |
| R3 | `collect_until_exit` 在 Bridge 单请求循环中会造成 head-of-line blocking | 9 | 评估用 `tokio::spawn` 隔离或标注限制 |
| R4 | CLI target 格式（`%N` vs `session:window.pane`）在各命令中是否一致 | 5, 6, 10, 11, 12 | 逐一测试 `-t %N` / `-s %N` 格式 |

---

## 概览

本文档列出 agent-ops 尚未独立封装的 rmux 原生功能，分为 🔴 高优先级（6 项）和 🟡 中优先级（7 项），共 **13 个待对接功能**。

每个功能需要涉及三个层面的改动：

| 层级 | 文件 | 改动内容 |
|------|------|----------|
| Bridge 协议 | `crates/rmux-bridge/src/protocol.rs` | 新增 `handle_xxx()` 方法 |
| Bridge 路由 | `crates/rmux-bridge/src/proxy.rs` | 新增 `"xxx"` → handler 映射 |
| MCP 工具 | `crates/agent-ops-mcp/src/main.rs` | 新增 tool definition（JSON Schema） |
| MCP 实现 | `crates/agent-ops-mcp/src/tools.rs` | 新增工具函数 + `execute_tool` 路由 |
| 审计 | `crates/agent-ops-core/src/types.rs` | 新增 `AuditAction` 变体 |

---

## 🔴 高优先级（6 项）

### 1. `find_panes` — Pane 发现

**rmux SDK API**: `Rmux::find_panes()` → `PaneFinder`

> `PaneFinder` 支持链式过滤：`session(name)`, `title(title)`, `title_prefix(prefix)`, `command_contains(needle)`, `cwd_contains(needle)`, `window_index(idx)`, `running()`, `exited()`  
> 最终调用 `all()` → `Vec<DiscoveredPane>`, `one()` → `Pane`, 或 `collect_paneset()` → `PaneSet`

**使用场景**：AI agent 需要找到"标题为 nginx-log 的 pane"或"运行着 postgres 的 pane"，而不是盲操作 `%0`。

**建议 MCP 工具**：

```json
{
  "name": "find_panes",
  "description": "Discover panes across sessions by title, command, working directory, or process state. Returns matching pane handles for further operations.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string", "description": "Hostname" },
      "session_name": { "type": "string", "description": "Filter by session name (optional)" },
      "title": { "type": "string", "description": "Exact pane title match" },
      "title_prefix": { "type": "string", "description": "Pane title prefix match" },
      "command_contains": { "type": "string", "description": "Filter panes whose command contains this string" },
      "cwd_contains": { "type": "string", "description": "Filter panes whose working directory contains this string" },
      "window_index": { "type": "integer", "description": "Filter by window index" },
      "running": { "type": "boolean", "description": "Only panes with running processes" },
      "exited": { "type": "boolean", "description": "Only panes with exited processes" }
    },
    "required": ["host"]
  }
}
```

**Bridge 协议 type**: `"find_panes"`

**返回**：
```json
{
  "ok": true,
  "panes": [
    {
      "pane_id": "%2",
      "session_name": "agent-ops",
      "session_id": "$1",
      "window_id": "@0",
      "window_index": 0,
      "pane_index": 0,
      "title": "nginx-log",
      "command": ["tail", "-f", "/var/log/nginx/access.log"],
      "working_directory": "/var/log/nginx",
      "process": "running",
      "pid": 12345,
      "tags": []
    }
  ],
  "count": 1
}
```

**实现要点**：
- Bridge 端将 `PaneFinder` 的链式调用转换为 JSON 参数驱动的过滤逻辑：根据传入参数条件性调用 `session()`, `title()`, `title_prefix()`, `command_contains()`, `cwd_contains()`, `window_index()`, `running()`, `exited()` 等方法，最后调用 `all()` 获取结果
- ⚠️ `running` 和 `exited` 是互斥的过滤条件，不应同时为 `true`（同时为 `true` 不会匹配任何 pane）
- `DiscoveredPane` 核心字段：`session_name`, `session_id`, `window_id`, `window_index`, `pane_id`, `pane_index`, `title`, `command`（`Vec<String>`）, `working_directory`, `tags`, `process`（`PaneProcessState` 枚举 — `Unknown` / `Running { pid }` / `Exited`）, `pane`（`Pane` handle）
- `process` 字段取值：`"unknown"` 表示进程状态未知；`"running"` 表示进程运行中（此时 `pid` 为可选的 OS 进程 ID）；`"exited"` 表示进程已退出
- 如需 `size_cols`/`size_rows`，需对每个 pane 额外调用 `Pane::info()`（不在 `DiscoveredPane` 中）

---

### 2. `find_sessions` — Session 发现

**rmux SDK API**: `Rmux::find_sessions()` → `SessionFinder`

> `SessionFinder` 支持 `name(name)` 过滤，最终调用 `all()` → `Vec<DiscoveredSession>` 或 `one()` → `Session`

**使用场景**：列出所有 session 及其基本信息，用于跨 session 运维。

**建议 MCP 工具**：

```json
{
  "name": "find_sessions",
  "description": "Discover sessions by exact name. Returns session handles for further operations.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string", "description": "Hostname" },
      "name": { "type": "string", "description": "Exact session name to find (optional, returns all if omitted)" }
    },
    "required": ["host"]
  }
}
```

**Bridge 协议 type**: `"find_sessions"`

**返回**：
```json
{
  "ok": true,
  "sessions": [
    {
      "session_name": "agent-ops"
    }
  ],
  "count": 1
}
```

**实现要点**：
- 如果 `name` 未指定，返回所有 session
- `DiscoveredSession` 仅含 `name: SessionName` + `session: Session`（live handle）
- 如需窗口数/pane 数，需进一步调用 `Session::window()` + `Window::panes()` 获取

**与现有 `session_list` 的区别**：
- `session_list` 也返回 `[{ "session_name": "..." }]`，但底层用 `Rmux::list_sessions()`（不返回 handle）
- `find_sessions` 底层用 `SessionFinder`，返回的 `DiscoveredSession` 包含 live `Session` handle，可进一步查询窗口和 pane 信息

> **决策**：不合并到 `session_list`。两者底层 API 不同（`list_sessions` vs `SessionFinder`），返回的 handle 能力也不同。`find_sessions` 更适合需要后续操作（如查询 pane）的场景。

---

### 3. `get_pane_title` — 读取 Pane 标题

**rmux SDK API**: `Pane::title()` → `Option<String>`

**使用场景**：现有 `set_pane_title` 只能写不能读，agent 需要验证标题是否设置成功，或在自动化流程中确认 pane 状态。

**建议 MCP 工具**：

```json
{
  "name": "get_pane_title",
  "description": "Get the current title of a pane. Returns null if no title is set.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"get_pane_title"`

**返回**：
```json
{
  "ok": true,
  "pane_id": "%0",
  "title": "nginx-log"
}
```

> 如果 pane 无标题，返回 `"title": null`

**实现要点**：单个 SDK 调用，无额外复杂性。可能在 `find_panes` 中已隐含此信息，但仍需独立工具方便单 pane 查询。

---

### 4. `find_text_all` — 全量文本匹配

**rmux SDK API**: `Pane::find_text_all(text)` → `Vec<PaneTextMatch>`

> 与 `find_text`（单匹配）不同，此 API 返回所有匹配位置，**包括同一行上的重叠匹配**。

**使用场景**：agent 需要统计终端输出中某个模式出现的所有位置（如找到所有 ERROR 行），或进行批量文本处理。

**建议 MCP 工具**：

```json
{
  "name": "find_text_all",
  "description": "Search pane visible text and return ALL match positions (including overlapping matches on the same line). Use find_pane_text for single-match queries.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "pattern": { "type": "string", "description": "Literal text to search for" }
    },
    "required": ["host", "session_name", "pane_id", "pattern"]
  }
}
```

**Bridge 协议 type**: `"find_text_all"`

**返回**：
```json
{
  "ok": true,
  "matches": [
    { "start_row": 5, "start_col": 0, "end_row": 5, "end_col": 5, "text": "ERROR" },
    { "start_row": 12, "start_col": 10, "end_row": 12, "end_col": 15, "text": "ERROR" }
  ],
  "count": 2
}
```

**实现要点**：与 `handle_find_pane_text` 几乎相同，仅将 `Pane::find_text()` 替换为 `Pane::find_text_all()`。

---

### 5. `clear_history` — 清空 Pane 滚动历史

**rmux CLI**: `clear-history -t <pane_id>`

> ⚠️ 此功能通过 `cmd_escape` 调用 rmux CLI，不是 SDK 方法

**使用场景**：agent 执行新命令前需要清空终端历史，避免 `capture_pane` 混入之前的输出。当前 `exec` 的 `clear_screen` 只清屏不清历史。

**建议 MCP 工具**：

```json
{
  "name": "clear_history",
  "description": "Clear the scrollback history of a pane. Unlike exec's clear_screen (which only clears the visible area), this removes all retained output.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"clear_history"`

**实现方式**：Bridge 端通过 `Rmux::cmd(&["clear-history", "-t", pane_id])` 执行。

**返回**：
```json
{ "ok": true }
```

---

### 6. 粘贴板操作（`list_buffers` + `paste_buffer` + `delete_buffer`）

**rmux CLI**: `list-buffers`, `paste-buffer`, `delete-buffer`

**使用场景**：AI agent 在 pane 间复制粘贴内容。典型工作流：
1. 在 pane A 中选择文本（通过 `send_keys` 进入 copy-mode）
2. 将选中内容保存到 buffer
3. 切换到 pane B，用 `paste_buffer` 粘贴

**建议 MCP 工具**：

#### 6a. `list_buffers`

```json
{
  "name": "list_buffers",
  "description": "List all paste buffers with their names and content previews.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" }
    },
    "required": ["host"]
  }
}
```

**Bridge 协议 type**: `"list_buffers"`

**返回**：
```json
{
  "ok": true,
  "buffers": [
    { "name": "buffer0", "size": 1024, "preview": "first 100 chars..." }
  ],
  "count": 1
}
```

#### 6b. `paste_buffer`

```json
{
  "name": "paste_buffer",
  "description": "Paste the content of a named paste buffer into a pane.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "buffer_name": { "type": "string", "description": "Buffer name, e.g. 'buffer0'. If omitted, pastes the top buffer." }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"paste_buffer"`

#### 6c. `delete_buffer`

```json
{
  "name": "delete_buffer",
  "description": "Delete a paste buffer by name.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "buffer_name": { "type": "string", "description": "Buffer name to delete" }
    },
    "required": ["host", "buffer_name"]
  }
}
```

**Bridge 协议 type**: `"delete_buffer"`

**实现方式**：所有 buffer 操作通过 `Rmux::cmd()` 执行 rmux CLI 命令。
- `list-buffers` → **需要解析 CLI 纯文本输出为结构化 JSON**（这是现有 Bridge handler 中从未有过的模式）。典型 tmux `list-buffers` 输出格式：`buffer0: 1024 bytes: "preview text..."`。Bridge 端需要用正则或行解析提取 `name`、`size`、`preview` 字段。**解析方案需在验证 R2（见上方「已知风险」）后确定**
- `paste-buffer -b <name> -t <pane_id>` → 粘贴到指定 pane
- `delete-buffer -b <name>` → 删除指定 buffer

> **注意**：rmux 的 buffer 操作参数格式可能与标准 tmux 有细微差异。Bridge 端需要在实际 rmux 环境中测试确认 CLI 参数格式（特别是 `-b`/`-t` 标志及 buffer 名称的引用方式）。

---

## 🟡 中优先级（7 项）

### 7. `split_pane_with` — 原子 Split + Spawn

**rmux SDK API**: `Pane::split_with(direction)` → `PaneSplitBuilder`

> `PaneSplitBuilder` 允许在 split 的同时指定新 pane 的进程，避免 split 后出现临时 shell → 再 spawn 的中间状态。

**使用场景**：agent 一键创建新 pane 并启动特定命令（如 `split_pane_with --command "htop"`），无需两步操作。

**建议 MCP 工具**：

```json
{
  "name": "split_pane_with",
  "description": "Split current pane and immediately spawn a command in the new pane (atomic operation). Avoids the intermediate default shell state of split_pane + spawn_command.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "direction": { "type": "string", "description": "horizontal or vertical" },
      "command": { "type": "string", "description": "Command to run in the new pane" },
      "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments" },
      "shell": { "type": "boolean", "description": "Run via /bin/sh -c (default: false, use argv)" }
    },
    "required": ["host", "session_name", "pane_id", "direction", "command"]
  }
}
```

**Bridge 协议 type**: `"split_pane_with"`

**返回**：
```json
{
  "ok": true,
  "new_pane_id": "%3"
}
```

**实现要点**：
- `PaneSplitBuilder` 支持 `.shell(command)` 和 `.spawn(argv)` 两种模式，返回 `Result<Pane>`
- `shell: true` → 使用 `/bin/sh -c`；`shell: false`（默认）→ 直接 exec argv
- `PaneSplitBuilder` 还支持 `.cwd(path)`, `.env(key, value)`, `.title(title)`, `.keep_alive_on_exit(bool)` 等方法，可作为后续增强参数

---

### 8. `get_pane_by_title` — 按标题查找单个 Pane

**rmux SDK API**: `Rmux::get_pane_by_title(title)` → `Pane`

**使用场景**：`find_panes` 返回列表，而 `get_pane_by_title` 是精确查找单 pane 的快捷方式，适合 agent 已知标题的场景。

**建议 MCP 工具**：

```json
{
  "name": "get_pane_by_title",
  "description": "Find a single pane by exact title. Returns an error if zero or multiple panes match.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "title": { "type": "string", "description": "Exact pane title to find" }
    },
    "required": ["host", "title"]
  }
}
```

**Bridge 协议 type**: `"get_pane_by_title"`

**返回**：
```json
{
  "ok": true,
  "found": true,
  "pane": {
    "pane_id": "%2",
    "session_name": "agent-ops",
    "window_index": 0,
    "pane_index": 0,
    "title": "nginx-log",
    "command": ["tail", "-f", "/var/log/nginx/access.log"],
    "working_directory": "/var/log/nginx",
    "process": "running",
    "pid": 12345,
    "tags": []
  }
}
```

**实现要点**：SDK 的 `get_pane_by_title` 内部使用 `find_panes().title(title).one()`。如果匹配数为 0 或多于 1，返回错误。返回字段与 `find_panes` 一致（来自 `DiscoveredPane`）。

---

### 9. `collect_until_exit` — 原生输出收集

**rmux SDK API**: `Pane::collect_output_until_exit(max_bytes)` → `CollectedPaneOutput`

> 收集 pane 从当前时刻到进程退出的所有原始输出字节。比当前 `exec` 的 send_keys+哨兵轮询方案更高效、更可靠。

**使用场景**：替代 `exec` 的实现方式，或用于需要精确输出收集的场景（如二进制输出、长时间运行的日志收集）。

**建议 MCP 工具**：

```json
{
  "name": "collect_until_exit",
  "description": "Collect raw pane output bytes from now until the pane process exits. More efficient than exec's sentinel-based polling for commands with large or complex output. The pane process must already be running — use spawn_command or exec to start it first.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "max_bytes": { "type": "integer", "description": "Max bytes to collect (default: 1048576 = 1MB)" },
      "timeout_ms": { "type": "number", "description": "Max wait time in ms (default: 60000)" },
      "starting_at": { "type": "string", "description": "Start position: 'now' (after newest output, default) or 'oldest' (from retained output start)" }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"collect_until_exit"`

**返回**：
```json
{
  "ok": true,
  "output": "<raw bytes as base64 string>",
  "collected_bytes": 4096,
  "exit_code": 0,
  "signal": null,
  "message": null,
  "truncated": false,
  "lagged": false,
  "missed_events": 0,
  "duration_ms": 1234
}
```

**实现要点**：
- Bridge 端调用 `Pane::collect_output_until_exit(max_bytes)` 或 `Pane::collect_output_until_exit_starting_at(PaneOutputStart::Now/Oldest, max_bytes)`
- `CollectedPaneOutput` 字段：`bytes: Vec<u8>`, `exit_state: Option<PaneExitState>`, `truncated: bool`, `lagged: bool`, `missed_events: u64`
- `PaneExitState` 包含 `code: Option<i32>`, `signal: Option<i32>`, `message: Option<String>`
- `PaneOutputStart::Now` 表示从最新输出之后开始（默认）；`PaneOutputStart::Oldest` 表示从保留的最旧输出开始
- `timeout_ms` 并非 SDK 原生参数 — Bridge 端需用 `tokio::time::timeout()` 包裹 SDK 调用实现超时控制
- ⚠️ **架构注意**：`collect_output_until_exit()` 是阻塞调用。Bridge 的 `proxy.rs` 采用单请求循环处理模式，此调用期间该 host 的 Bridge **不会处理其他 MCP 请求**（head-of-line blocking）。建议用 `tokio::spawn` 将 SDK 调用放入独立 task，超时后 cancel：
  ```rust
  let handle = tokio::spawn(async { pane.collect_output_until_exit(max).await });
  match tokio::time::timeout(timeout, handle).await {
      Ok(Ok(output)) => { /* 正常返回 */ }
      Ok(Err(e)) => { /* SDK 错误 */ }
      Err(_) => { handle.abort(); /* 超时 */ }
  }
  ```
- Bridge 端需对 `bytes` 做 base64 编码（当前 MCP content 类型仅支持文本）
- **注意**：这不是 `exec` 的替代品 — pane 进程需已在运行（通过 `spawn_command` 或 `exec` 启动）

---

### 10. `break_pane` — 将 Pane 拆分为独立窗口

**rmux CLI**: `break-pane [-d] [-s <source_pane>] [-t <dst_window>]`

> 在 tmux/rmux 中，`-s` 指定要拆出的源 pane（默认当前 active pane），`-t` 指定目标窗口（默认创建新窗口），`-d` 表示不切换焦点。

**使用场景**：agent 需要将某个 pane 升级为独立窗口（如想在全屏模式下查看某个 pane 的输出）。

**建议 MCP 工具**：

```json
{
  "name": "break_pane",
  "description": "Break a pane out of its current window into a new window, or move it to another window.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string", "description": "Source pane to break out (default: current active pane)" },
      "destination_window": { "type": "integer", "description": "Destination window index. If omitted, creates a new window." },
      "detached": { "type": "boolean", "description": "Don't focus the new window after breaking (default: false)" }
    },
    "required": ["host", "session_name"]
  }
}
```

**Bridge 协议 type**: `"break_pane"`

**实现方式**：Bridge 端构造 CLI 参数：
- 仅 break 到新窗口：`Rmux::cmd(&["break-pane", "-s", pane_id])`  或加 `-d`
- break 到指定窗口：`Rmux::cmd(&["break-pane", "-s", pane_id, "-t", &format!(":{}", dest_window)])`

**返回**：
```json
{
  "ok": true,
  "pane_id": "%1",
  "window_index": 1
}
```

---

### 11. `join_pane` — 合并 Pane 到指定窗口

**rmux CLI**: `join-pane [-bdhv] [-l size] -s <src_pane> -t <dst_pane>`

> `-s` 指定要移动的源 pane，`-t` 指定目标 pane（新 pane 将拆分到目标 pane 旁边），`-h` 水平拆分，`-v` 垂直拆分，`-l` 指定新 pane 尺寸。

**使用场景**：将孤立窗口中的 pane 合并回主窗口，整理终端布局。

**建议 MCP 工具**：

```json
{
  "name": "join_pane",
  "description": "Move a pane into another window, splitting next to the target pane.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "source_pane_id": { "type": "string", "description": "Source pane to move" },
      "target_pane_id": { "type": "string", "description": "Target pane — source will be placed next to this pane" },
      "direction": { "type": "string", "description": "Split direction: 'horizontal' (-h) or 'vertical' (-v). Default: vertical." },
      "size": { "type": "integer", "description": "Size of the new pane in rows (horizontal) or cols (vertical). Omit for even split." }
    },
    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
  }
}
```

**Bridge 协议 type**: `"join_pane"`

**实现方式**：Bridge 端构造 CLI 参数：
```
Rmux::cmd(&["join-pane", "-s", src, "-t", dst, flag, ...])
```
- `direction: "horizontal"` → 添加 `-h` 标志；`direction: "vertical"` → 添加 `-v` 标志
- `size` → 添加 `-l <size>` 参数

**返回**：
```json
{
  "ok": true,
  "source_pane_id": "%1",
  "target_pane_id": "%2"
}
```

---

### 12. `swap_pane` — 交换两个 Pane 位置

**rmux CLI**: `swap-pane -s <src_pane> -t <dst_pane> [-dDU]`

**使用场景**：agent 重新排列终端布局，将常用 pane 换到显眼位置。

**建议 MCP 工具**：

```json
{
  "name": "swap_pane",
  "description": "Swap two panes (exchange their positions in the window layout).",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "source_pane_id": { "type": "string", "description": "Source pane ID" },
      "target_pane_id": { "type": "string", "description": "Target pane ID" },
      "detached": { "type": "boolean", "description": "Don't change active pane after swap (default: false)" }
    },
    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
  }
}
```

**Bridge 协议 type**: `"swap_pane"`

**实现方式**：通过 `Rmux::cmd(&["swap-pane", "-s", src, "-t", dst])` 执行。

**返回**：
```json
{
  "ok": true,
  "source_pane_id": "%0",
  "target_pane_id": "%2"
}
```

---

### 13. `capabilities` — Daemon 能力查询

**rmux SDK API**: `Rmux::capabilities()` → `Vec<String>`

**使用场景**：agent 在执行某些操作前判断 daemon 是否支持（如 web share、stream_pane 等高级功能），避免失败后再报错。

**建议 MCP 工具**：

```json
{
  "name": "host_capabilities",
  "description": "Query the rmux daemon capabilities on a host. Useful for checking feature availability before attempting advanced operations.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "check": { "type": "string", "description": "Specific capability to check (optional). If omitted, returns all capabilities." }
    },
    "required": ["host"]
  }
}
```

**Bridge 协议 type**: `"capabilities"`

**返回**：
```json
{
  "ok": true,
  "capabilities": ["web-share", "control-mode", "sixel", "kitty-keyboard"],
  "count": 4
}
```

> 如果指定了 `check`，额外返回 `"has_capability": true` 字段。

**实现要点**：
- Bridge 端调用 `Rmux::capabilities()` 或 `Rmux::has_capability(check)` 
- 能力列表中的值由 rmux daemon 在握手时协商，如 `"web-share"`, `"control-mode"`, `"sixel"` 等

---

## 实现影响面汇总

### 新增 AuditAction 变体

在 `crates/agent-ops-core/src/types.rs` 的 `AuditAction` 枚举中新增：

```rust
FindPanes,
FindSessions,
GetPaneTitle,
FindTextAll,
ClearHistory,
ListBuffers,
PasteBuffer,
DeleteBuffer,
SplitPaneWith,
GetPaneByTitle,
CollectUntilExit,
BreakPane,
JoinPane,
SwapPane,
HostCapabilities,
```

### 新增 Bridge 协议 type

在 `crates/rmux-bridge/src/proxy.rs` 的 `match req_type` 中新增 **15 个**分支（6 高优 + 7 中优 + 3 buffer 子工具）：

```
find_panes, find_sessions, get_pane_title, find_text_all,
clear_history, list_buffers, paste_buffer, delete_buffer,
split_pane_with, get_pane_by_title, collect_until_exit,
break_pane, join_pane, swap_pane, capabilities
```

### 新增 Bridge handler

在 `crates/rmux-bridge/src/protocol.rs` 中新增对应的 15 个 `handle_xxx()` 方法。

### SDK 依赖

当前已依赖的 rmux-sdk 类型足以支持所有新功能。如发现缺失的类型，需要在 proto-col 中 `use` 导入。

### rmux CLI 依赖

以下功能依赖 `Rmux::cmd()` 执行 rmux CLI（非 SDK 方法）：
- `clear_history`
- `list_buffers` / `paste_buffer` / `delete_buffer`
- `break_pane` / `join_pane` / `swap_pane`

> ⚠️ 需要验证目标 rmux 版本是否支持这些 CLI 子命令及参数格式。

---

## 错误处理约定

所有新增功能遵循以下统一约定：

### 空结果

| 场景 | 返回 |
|------|------|
| 查询无匹配（`find_panes`/`find_sessions` 等） | `{"ok": true, "items": [], "count": 0}` |
| 单对象查询无匹配（`get_pane_by_title`） | `{"ok": false, "found": false, "error": "no pane found with title: X"}` |
| Pane 无标题（`get_pane_title`） | `{"ok": true, "title": null}` |

### CLI 命令失败

对于依赖 `Rmux::cmd()` 的功能，CLI 返回非零 exit_code 时：

```json
{
  "ok": false,
  "error": "CLI command 'clear-history' exited with code 1: <stderr output>"
}
```

### 超时

| 场景 | 返回 |
|------|------|
| 操作超时（`collect_until_exit` 等） | `{"ok": false, "error": "timeout after Nms"}` |

### 守护进程不支持

| 场景 | 返回 |
|------|------|
| 某操作不被 daemon 版本支持 | `{"ok": false, "error": "command not supported by rmux daemon: X"}` |

### Pane/Session 不存在

与现有 handler 保持一致：
```json
{"ok": false, "error": "pane not found: %X"}
{"ok": false, "error": "session not found: Y"}
```

### 互斥参数

| 场景 | 处理 |
|------|------|
| `find_panes` 的 `running=true, exited=true` 同时传入 | Bridge 端拒绝请求：`{"ok": false, "error": "running and exited are mutually exclusive"}` |

---

## 阶段规划建议

### Phase 1：基础发现与查询（4 项，无破坏性）

| # | 功能 | 依赖 | 风险 |
|---|------|------|------|
| 1 | `find_panes` | 纯 SDK | 低 |
| 2 | `find_sessions` | 纯 SDK | 低 |
| 3 | `get_pane_title` | 纯 SDK | 低 |
| 4 | `find_text_all` | 纯 SDK（与 `find_text` 几乎相同） | 极低 |

### Phase 2：交互增强（3 项）

| # | 功能 | 依赖 | 风险 |
|---|------|------|------|
| 5 | `clear_history` | rmux CLI | 中（需验证 CLI 参数） |
| 6 | `list_buffers` / `paste_buffer` / `delete_buffer` | rmux CLI | 中（需验证 CLI 参数） |
| 7 | `split_pane_with` | 纯 SDK | 低 |

### Phase 3：布局操作 + 诊断（4 项）

| # | 功能 | 依赖 | 风险 |
|---|------|------|------|
| 8 | `get_pane_by_title` | 纯 SDK | 低 |
| 9 | `collect_until_exit` | SDK（现有 `output_stream` 已有先例） | 中 |
| 10 | `break_pane` / `join_pane` / `swap_pane` | rmux CLI | 中 |
| 11 | `capabilities` | 纯 SDK | 极低 |

### 可选增强（第二批 — 待实现）

> 状态：已实现 | 最后更新：2026-07-04  
> 基于 rmux-sdk 0.7.1 完整 API 与 agent-ops 现有 57 工具对比分析

#### 14. `capture_region` — 矩形区域捕获（含全屏截图）

**rmux SDK API**: `Pane::capture_region(Rect)` / `Pane::screenshot()` → `CaptureBuilder`

> 捕获 pane 的矩形区域。不传坐标时为全屏截图（等价于 `screenshot()`）。`CaptureBuilder` 支持 `.preserve_style(bool)`。

**使用场景**：从表格输出中提取特定列、截取终端 UI 中的矩形块、全屏终端截图。

**建议 MCP 工具**：

```json
{
  "name": "capture_region",
  "description": "Capture a rectangular region of a pane, or the full pane screenshot when no coordinates are specified. Supports plain text or styled (color markup) output.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "row": { "type": "integer", "description": "Top row (0-based). Omit all coords for full screenshot." },
      "col": { "type": "integer", "description": "Left column (0-based)" },
      "rows": { "type": "integer", "description": "Height in rows" },
      "cols": { "type": "integer", "description": "Width in columns" },
      "styled": { "type": "boolean", "description": "Preserve style/color markup (default: false, plain text)" }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"capture_region"`

**返回**：
```json
{
  "ok": true,
  "text": "captured content...",
  "styled": false
}
```

错误格式：
```json
{ "ok": false, "error": "all 4 coordinates required (row, col, rows, cols), or omit all for full screenshot" }
{ "ok": false, "error": "rows and cols must be non-zero" }
```

**实现要点**：
- SDK `Rect` 结构：`Rect { row: u16, col: u16, rows: u16, cols: u16 }`（**起点+高宽**，不是起点+终点）
- MCP 传整数 → Bridge 端 `u16::try_from(val).map_err(...)` 转换
- 坐标全指定 → `pane.capture_region(Rect { row, col, rows, cols }).preserve_style(styled).await?`
- 坐标全未指定 → `pane.screenshot().preserve_style(styled).await?`
- 部分坐标 → 报错
- `rows == 0 || cols == 0` → 拒绝（空捕获无意义）
- 返回 `Result<CapturedRegion>`，`.text()` 获取纯文本字符串
- `CaptureBuilder` 与 `PaneCaptureBuilder` 是**两个不同类**：前者走 snapshot 实时抓取，后者走 daemon `capture-pane` 协议命令
- Import: `use rmux_sdk::capture::{CaptureBuilder, CapturedRegion, Rect};`
- 大终端可能产生大量输出（200×100 = 20K 字符），Bridge 端 `MAX_FRAME_SIZE` (64MB) 已足够覆盖

---

#### 15. `wait_for_bytes` — 原始字节流等待

**rmux SDK API**: `Pane::wait_for(bytes)` / `Pane::wait_for_next(bytes)` → `ArmedWait`

> 等待原始输出流中出现特定字节序列。`wait_for` 包含历史数据，`wait_for_next` 仅匹配新数据。

**使用场景**：等待二进制标志、ANSI escape 序列、或无法用 `wait_for_text`（可见文本）匹配的控制序列。

**建议 MCP 工具**：

```json
{
  "name": "wait_for_bytes",
  "description": "Wait for specific raw bytes to appear in the pane output stream. Unlike wait_for_text (which only matches visible text), this matches the raw output including ANSI sequences and control characters.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "bytes": { "type": "string", "description": "Raw bytes to wait for (as base64-encoded string)" },
      "only_new": { "type": "boolean", "description": "Only match data appearing after this call (skip history, default: false)" },
      "timeout_ms": { "type": "number", "description": "Default 30000" }
    },
    "required": ["host", "session_name", "pane_id", "bytes"]
  }
}
```

**Bridge 协议 type**: `"wait_for_bytes"`

**返回**：
```json
{
  "ok": true,
  "found": true
}
```

超时返回（与 `wait_for_text` 一致）：
```json
{ "ok": false, "found": false, "error": "timeout waiting for bytes after Nms" }
```

**实现要点**：
- `bytes` 参数为 base64 编码（MCP JSON 无法直接传二进制）
- Bridge 端 decode base64 → 调用 SDK 的 `wait_for` 或 `wait_for_next`
- **`wait_for` / `wait_for_next` API 签名**（需验证实际 SDK 版本）：
  ```rust
  // 路径 A (only_new=false): wait_for — 等待字节出现（含历史），直接 await 即可
  pane.wait_for(&decoded_bytes).await?                    // → Result<()>
  // 路径 B (only_new=true): wait_for_next — 仅等待新数据，两步 await
  let armed = pane.wait_for_next(&decoded_bytes).await?;  // → Result<ArmedWait>
  let () = armed.await?;                                  // ArmedWait.await → Result<()>
  ```
  Bridge 端需根据 `only_new` 参数分支处理两种不同的调用链
- `ArmedWait` 构造时即绑定 timeout（来自 SDK 默认或 daemon 配置），**实际等待超时由 SDK 内建机制管理，不由 MCP `timeout_ms` 参数控制**。`timeout_ms` 作为上层预留参数（用于未来 SDK 版本支持显式超时接口），当前实现中 Bridge 端将其透传但不实际控制 ArmedWait 的超时行为
- ⚠️ `ArmedWait` 已有内建 timeout，**不能**外套 `tokio::time::timeout`（双重超时竞态）
- SDK 内建空字节检查（`bytes.is_empty()` 自动返回错误），Bridge 端无需追加
- 无效 base64 → `{"ok": false, "error": "invalid base64: ..."}`

---

#### 16. `wait_stable` — 等待输出稳定

**rmux SDK API**: `Pane::wait_until_stable_for(duration)`

> 等待 pane 输出在经过 `duration` 毫秒无变化后确认稳定。适合在命令执行完后等终端渲染完成。

**使用场景**：`exec` 或 `send_keys` 执行命令后，等终端渲染稳定再 `capture_pane`。

**建议 MCP 工具**：

```json
{
  "name": "wait_stable",
  "description": "Wait until the pane output has been stable (no changes) for a given duration. Useful after sending commands to ensure terminal rendering is complete before capturing output.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string" },
      "session_name": { "type": "string" },
      "pane_id": { "type": "string" },
      "stable_ms": { "type": "number", "description": "Duration of stability required in ms (default: 500)" },
      "timeout_ms": { "type": "number", "description": "Max total wait time in ms (default: 30000)" }
    },
    "required": ["host", "session_name", "pane_id"]
  }
}
```

**Bridge 协议 type**: `"wait_stable"`

**返回**：
```json
{
  "ok": true,
  "stable": true
}
```

超时返回：
```json
{ "ok": false, "stable": false, "error": "timeout: pane did not stabilize within Nms" }
```

**实现要点**：
- SDK 返回 `TerminalLoadStateWait`（builder 类型），**不是直接可 await 的 Future**
- 实际调用链：`pane.wait_until_stable_for(Duration::from_millis(stable_ms)).timeout(Duration::from_millis(timeout_ms)).await`
  - `wait_until_stable_for(dur)` → `TerminalLoadStateWait`
  - `.timeout(dur)` → `TerminalLoadStateWait`（设置总体超时，返回自身）
  - `.await` → `Result<(), RmuxError>`（超时返回 `Err`）
- `.timeout()` 方法设置总体超时；若不调用，使用 SDK 默认超时
- ⚠️ **不能**外套 `tokio::time::timeout`（`TerminalLoadStateWait` 已有内建超时，双重包裹会竞态）
- `stable_ms <= 0` 应拒绝（立即返回无意义），Bridge 端自行校验
- Import: `TerminalLoadStateWait` 在 `rmux_sdk::` 根路径 re-export

---

#### 17. `split_pane_with` 参数增强

**SDK API**: `PaneSplitBuilder` 的 `.cwd()`, `.env()`, `.title()`, `.keep_alive_on_exit()`

> 当前 `split_pane_with` 只传 `command` + `args` + `shell`。增加以下可选参数。

**新增 MCP 参数**：

| 参数 | 类型 | SDK 方法 | 说明 |
|------|------|----------|------|
| `cwd` | string | `.cwd(path)` | 新 pane 的工作目录 |
| `env` | object | `.env(k, v)` | 环境变量键值对（JSON object 的每个属性值必须为 string） |
| `title` | string | `.title(title)` | 新 pane 的标题 |
| `keep_alive_on_exit` | boolean | `.keep_alive_on_exit(bool)` | 进程退出后保留 pane（不自动关闭） |

**实现要点**：
- 在 `handle_split_pane_with` 中根据参数存在性条件调用 builder 方法：
  ```rust
  let mut builder = pane.split_with(dir);
  if shell { builder = builder.shell(command); }          // shell 模式忽略 args
  else { builder = builder.spawn(argv); }
  if let Some(cwd) = cwd { builder = builder.cwd(PathBuf::from(cwd)); }
  if let Some(env) = env { for (k, v) in env { builder = builder.env(k, v); } }
  if let Some(title) = title { builder = builder.title(title); }
  if let Some(keep) = keep_alive_on_exit { builder = builder.keep_alive_on_exit(keep); }
  let new_pane = builder.await?;
  ```
- proxy.rs 需从 JSON request 中提取新字段（`cwd`, `env`, `title`, `keep_alive_on_exit`）
- `env` 为 JSON object（`serde_json::Value::Object`），需遍历键值对逐个调用 `.env(k, v)`

---

#### 18. `respawn_pane` 参数增强

**SDK API**: `PaneRespawnOptions` 的字段

> 当前 `respawn_pane` 使用 `PaneRespawnOptions::default()`，所有字段均为默认值。`PaneRespawnOptions` 结构：

```rust
pub struct PaneRespawnOptions {
    pub kill: bool,                          // 是否杀运行中进程（默认 false）
    pub start_directory: Option<PathBuf>,     // 工作目录
    pub process: ProcessSpec,                // argv + env
    pub keep_alive_on_exit: Option<bool>,    // 退出后保留 pane
}
```

**新增 MCP 参数**：

| 参数 | 类型 | SDK 字段 | 说明 |
|------|------|----------|------|
| `command` | string | `process.process_command` (Argv/Shell) | 替换默认 shell 的命令 |
| `args` | string[] | `process.process_command` (Argv) | 命令参数 |
| `shell` | boolean | — | 通过 `/bin/sh -c` 执行 |
| `cwd` | string | `start_directory` | 工作目录 |
| `env` | object | `process.environment` | 环境变量（`"KEY=VALUE"` 格式） |
| `kill` | boolean | `kill` | 强杀运行中进程（默认 false） |
| `keep_alive_on_exit` | boolean | `keep_alive_on_exit` | 退出后保留 pane |

**实现要点**：
- 如果 `command` 未提供，保留现有行为（使用 `PaneRespawnOptions::default()` + 默认 shell）
- `kill: true` 解决了当前"pane still active"错误，可强制替换运行中进程
- **`ProcessSpec` 实际结构**：
  ```rust
  pub struct ProcessSpec {
      pub process_command: Option<ProcessCommandSpec>,
      pub environment: Option<Vec<String>>,  // "KEY=VALUE" 格式
  }
  pub enum ProcessCommandSpec {
      Argv(Vec<String>),      // 直接 exec argv
      Shell(String),          // 通过 /bin/sh -c 执行
  }
  ```
- `shell: true` → `ProcessCommandSpec::Shell(command.into())`
- `shell: false`（默认）→ `ProcessCommandSpec::Argv(vec![cmd, ..args])`
- `env` JSON object `{"KEY": "VALUE"}` 需转为 `Vec<String>` 如 `["KEY=VALUE"]`
- `env` 为空 object `{}` 时不应设置 `process.environment`（避免空环境变量覆盖继承环境）

---

#### 19. `capture_pane` 参数增强

**SDK API**: `PaneCaptureBuilder` 的多种选项

> 当前 `capture_pane` 基于 `snapshot().visible_text()` + `clean_text()`。增加参数切换到 `PaneCaptureBuilder` 路径，获取更丰富的捕获能力。

**新增 MCP 参数**：

| 参数 | 类型 | SDK 方法 | 说明 |
|------|------|----------|------|
| `ansi` | boolean | `.escape_ansi(true)` | 保留 ANSI 颜色/样式码 |
| `start_line` | integer | `.start(line)` | 起始行（负=从末尾倒数历史行）。同时指定 `max_lines` 时优先生效 |
| `end_line` | integer | `.end(line)` | 结束行（负=从末尾倒数历史行）。配合 `start_line` 指定精确范围 |
| `join_wrapped` | boolean | `.join_wrapped(true)` | 将终端自动换行合并为一行 |
| `preserve_spaces` | boolean | `.preserve_trailing_spaces(true)` | 保留行尾空格（默认去除） |
| `alternate` | boolean | `.alternate(true)` | 捕获 alternate screen（如 vim/less 的全屏界面） |
| `buffer_name` | string | `.buffer(name)` | 写入 paste buffer 而非返回。与其他参数互斥 |

**实现要点**：

⚠️ **双路径设计（关键兼容性保证）**：
- **路径 A（现有）**：仅 `max_lines` 或 `max_lines=200` 且无其他高级参数 → 保持现有 `snapshot().visible_text()` + `clean_text()` + 截尾 N 行。**行为完全不变**
- **路径 B（新）**：`ansi`/`alternate`/`start_line`/`end_line`/`buffer_name`/`join_wrapped`/`preserve_spaces` 任一指定 → 切换到 `PaneCaptureBuilder` 路径
- ⚠️ **路径 A 和路径 B 输出语义不同**：路径 A 经 `clean_text` 过滤 prompt 行和 ANSI；路径 B 返回 `capture-pane` 命令的原始 stdout（可能含 prompt、ANSI）。这是两种底层机制的本质差异。文档中 `max_lines` 走路径 A 保证兼容，高级参数走路径 B 获取新能力

⚠️ **ANSI 输出的编码**：`PaneCapture.stdout` 是 `Vec<u8>`。`PaneCapture` 结构体：
```rust
pub struct PaneCapture {
    pub stdout: Vec<u8>,
    pub buffer_name: Option<String>,
}
```
当 `ansi: true`（SDK `escape_ansi: true`）时，`stdout` 可能含 null 字节和无效 UTF-8。bridge 端需对 `stdout` 做 base64 编码。返回中 `text` 为 base64，同时增加 `encoding: "base64"` 字段标记。`ansi: false` 时 `encoding` 不存在，`text` 为普通 UTF-8。

> 与 `collect_until_exit` 的对比：`collect_until_exit` 返回的 `output` 字段**始终**为 base64（因收集的是原始字节流，必定二进制安全），故无需 `encoding` 标记。`capture_pane` 增强仅在 `ansi: true` 时需要 base64，其余情况为纯文本，因此用独立的 `encoding` 字段区分。

⚠️ **`buffer_name` 互斥**：指定 `buffer_name` 后输出写入 daemon buffer，`PaneCapture.stdout` 为空。需在返回中明确标注 `"buffer_written": "buffer_name"`。

⚠️ **`max_lines` + 高级参数混用**：`max_lines` 在路径 B 中等价于 `start_line = -(max_lines as i64)`。若同时提供了 `start_line`，`start_line` 优先生效（不静默覆盖）。`max_lines=0` 在路径 B 中表示全量捕获（不设 start_line/end_line）。

- 路径 A 保持 `clean_text` prompt 过滤；路径 B 走 `PaneCaptureBuilder` 不经过 `clean_text`（如需 prompt 过滤，后续在 MCP 侧处理）

**路径 B 返回格式**（与路径 A 兼容）：
```json
{
  "ok": true,
  "text": "captured output...",
  "encoding": null,
  "buffer_written": null
}
```
- 走路径 B 且未指定 `buffer_name` → `text` 为 capture 输出；`buffer_written: null`
- 走路径 B 且指定了 `buffer_name` → `text: ""`，`buffer_written: "name"`
- 走路径 B 且 `ansi: true` → `text` 为 base64，`encoding: "base64"`
- 走路径 A（无高级参数）→ 行为完全不变，返回格式不变（无 `encoding` / `buffer_written` 字段）

---

### 实现影响面汇总（第二批）

#### 新增 AuditAction 变体

```rust
CaptureRegion,
WaitForBytes,
WaitStable,
```

> 参数增强（17-19）不新增 AuditAction，复用现有 `SplitPaneWith`、`RespawnPane`、`CapturePane`。

#### 新增 Bridge 协议 type

```
capture_region, wait_for_bytes, wait_stable
```

> ⚠️ **三层修改提醒**：参数增强（#17-#19）不仅需要修改 proxy.rs 和 protocol.rs，**`tools.rs` 的对应函数也需要将新参数打包进发送给 Bridge 的 JSON frame**。例如 `split_pane_with` 的 tools.rs 函数需要增加 `cwd`, `env`, `title`, `keep_alive_on_exit` 到 `json!({...})` 中。现有设计文档中"实现要点"仅覆盖 Bridge 侧，MCP 侧同步修改不可遗漏。

#### SDK 依赖

新增 import：
- `CaptureBuilder`, `CapturedRegion`, `Rect`（`rmux_sdk::capture`）
- `TerminalLoadStateWait`（`rmux_sdk`，`wait_until_stable_for` 返回类型）
- `ProcessCommandSpec`（`rmux_sdk::ProcessCommandSpec`，enum `{ Argv, Shell }`）
- `PaneRespawnOptions`（已有 import，需改为构造非默认值）
- `PaneCaptureBuilder`, `PaneCapture`（`rmux_sdk::PaneCaptureBuilder`, `rmux_sdk::PaneCapture`）

注意：
- `Rect` 字段为 `{ row, col, rows, cols }`（起点+高宽），不是 `{ start_row, start_col, end_row, end_col }`（起点+终点）
- `ProcessSpec` 使用 `ProcessCommandSpec` 枚举而非直接 `Vec<String>`

#### 风险标注

| # | 功能 | 类型 | 风险 |
|---|------|------|:---:|
| 14 | `capture_region`（含全屏截图） | 纯 SDK | 低 |
| 15 | `wait_for_bytes` | SDK（含 base64 decode） | 低 |
| 16 | `wait_stable` | 纯 SDK | 极低 |
| 17 | `split_pane_with` 增强 | 扩展现有 handler | 极低 |
| 18 | `respawn_pane` 增强 | 扩展现有 handler | 低 |
| 19 | `capture_pane` 增强 | 扩展现有 handler（双路径设计） | **中** |

---

### 原有可选增强（不纳入本次计划）

| 功能 | 理由 |
|------|------|
| `line_stream` / `render_stream` | MCP stdio 协议不支持服务端推送，需架构级重构 |
| Web Share 全套 | 运维场景几乎用不到 |
| `Locator` API (`click`, `hover`, `fill`) | 链式调用 API 不适合 MCP tool schema |
| `PaneSet` (batch pane ops) | 需要 MCP 侧管理 pane 集合，复杂度高 |
| `Session::layout()` (Grid Layout) | 声明式布局 API 复杂，`split_pane` 已覆盖 |

---

## 参考

- [rmux-sdk 0.7.1 docs.rs](https://docs.rs/rmux-sdk/0.7.1/)
- [agent-ops 现有工具文档](TOOLS.md)
- [Bridge 协议设计](../crates/rmux-bridge/src/protocol.rs)
- [MCP 工具实现](../crates/agent-ops-mcp/src/tools.rs)
