# agent-ops MCP 工具文档

> agent-ops 是一个 MCP Server，使 AI Agent 能够通过 RMUX SDK 远程控制 Linux 主机的交互式终端会话。所有工具通过 `host` 参数路由到目标主机。

## 约定

- `host` — 主机名，对应 `config/hosts.yaml` 中的 `name` 字段
- `session_name` — 会话名（如 `s1`、`agent`）
- `pane_id` — 窗格 ID（如 `%0`、`%4`）
- `window_index` — 窗口索引，从 0 开始
- `timeout_ms` — 超时毫秒数，默认 30000
- 返回值统一为 JSON：`{"ok": true/false, ...}`

---

## 主机管理

### `host_list`

列出所有已注册主机。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| 无 | | |

**返回** `{"hosts": [...], "count": N}`

### `host_filter`

按条件过滤主机。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `group` | string | | 按分组过滤 |
| `tags` | string[] | | 同时匹配所有 tag |
| `label_key` | string | | 标签键 |
| `label_value` | string | | 标签值 |
| `pattern` | string | | 主机名 glob 模式，如 `prod-web-*` |

---

## 会话管理

### `session_create`

在指定主机上创建新的终端会话。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `session_name` | string | | 会话名称（可选，默认 `agent-session`） |

**返回** `{"ok": true, "session_name": "...", "pane_id": "%N"}`

### `session_list`

列出指定主机上的所有活动会话。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |

**返回** `{"ok": true, "sessions": [{"session_name": "..."}]}`

### `session_attach`

检查会话是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

> **注意**：当前仅检查存在性，不执行真正的 attach。

### `session_detach`

检查会话是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

> **注意**：当前仅检查存在性，不执行真正的 detach。

---

## 终端输入

### `send_keys`

向窗格发送按键。用于特殊键（Ctrl-C=`\x03`、Enter=`\n`、Tab=`\t`、Escape=`\x1b` 等）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |
| `keys` | string | ✅ |

### `send_text`

向窗格发送字面文本。与 `send_keys` 区别：`send_text` 发送纯文本，`send_keys` 额外支持特殊键 token。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |
| `text` | string | ✅ |

---

## 终端输出

### `capture_pane`

捕获窗格文本（默认最后 200 行，`max_lines=0` 返回全部 scrollback）。经 ANSI 剥离和 prompt 过滤。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `max_lines` | integer | | 默认 200，0=不限制 |

**返回** `{"ok": true, "text": "..."}`

### `wait_for_text`

等待窗格中出现指定文本（带超时）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `text` | string | ✅ | 等待出现的文本 |
| `timeout_ms` | number | | 等待超时毫秒数，默认 30000 |

**返回** `{"ok": true, "found": true}`

### `stream_pane`

> ⚠️ **当前不可用** — MCP 请求-响应协议不支持服务端持续推送流。bridge 的流推送功能本身正常，但 MCP Server 在返回初始响应后即断开 TLS 连接，无法转发流数据。
>
> 替代方案：用 `send_keys` 发送命令，然后用 `capture_pane` 多次轮询获取新输出。

### `find_pane_text`

在窗格可见文本中搜索，返回第一个匹配的位置。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |
| `pattern` | string | ✅ |

**返回** `{"ok": true, "found": true, "match": {"start_row": 0, "start_col": 0, "end_row": 0, "end_col": 4, "text": "root"}}`

---

## 命令执行

### `exec`

一站式命令执行。内部自动完成：清屏 → 发送命令 → 等待完成 → 捕获输出 → 清洗。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `session_name` | string | ✅ | 会话名称 |
| `pane_id` | string | ✅ | 窗格 ID，如 `%4` |
| `command` | string | ✅ | shell 命令 |
| `timeout_ms` | number | | 命令执行超时毫秒数，默认 30000 |
| `max_lines` | integer | | 默认 200，0=不限制 |
| `clear_screen` | boolean | | 执行前是否清屏，默认 false |

**返回** `{"ok": true, "output": "...", "exit_code": 0, "duration_ms": 70}`

超时：`{"ok": false, "output": "...", "exit_code": null, "error": "timeout..."}`

✅ 适用：一次性会自行退出的命令（`ls`、`cat`、`grep`、`systemctl`、`kubectl`、`apt-get`、`curl` 等）
❌ 不适用：交互式程序（`vim`、`htop`、`less`）、不自动退出的命令（`tail -f`、`nc -l`、`ping`）

> 对不自动退出的命令，请用 `send_keys` 发送，然后用 `wait_for_text`/`capture_pane` 观察输出，用 `send_keys("\x03")` 发送 Ctrl-C 中断。

### `wait_exit`

等待窗格中进程退出并返回退出状态。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `timeout_ms` | number | | 超时毫秒数，默认 30000 |

**返回** `{"ok": true, "exited": true, "exit_code": 0, "signal": null}`

---

## 窗格操作

### `split_pane`

在当前窗格内分屏，返回新 pane 的 ID。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | 要分割的窗格 ID |
| `direction` | string | ✅ | `vertical`（左右分屏）或 `horizontal`（上下分屏） |

**返回** `{"ok": true, "pane_id": "%N"}` — 新 pane 的 ID

### `split_window`

在会话中创建新窗口（非 pane 分屏）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `direction` | string | | `horizontal` 或 `vertical`（当前无效，仅兼容保留） |

> **注意**：`split_window` 创建全新空 window，不含额外 pane。如需 pane 级别的左右/上下分屏，请用 `split_pane`。

### `resize_pane`

调整窗格尺寸（列数 × 行数）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|:---:|:---:|------|
| `host` | string | ✅ | | |
| `session_name` | string | ✅ | | |
| `pane_id` | string | ✅ | | |
| `cols` | integer | | 80 | 列数（宽度） |
| `rows` | integer | | 24 | 行数（高度） |

### `set_pane_title`

设置窗格标题。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |
| `title` | string | ✅ |

### `close_pane`

关闭窗格（杀死 pane 进程）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | 窗格 ID，如 `%4` |

**返回** `{"ok": true, "closed": true}`

---

## 窗口操作

### `close_window`

关闭窗口（杀死其中所有 pane）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "closed": true}`

### `rename_window`

重命名窗口。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |
| `name` | string | ✅ |

### `resize_window`

调整窗口尺寸。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `window_index` | integer | ✅ | |
| `width` | integer | | 宽度（可选） |
| `height` | integer | | 高度（可选） |

### `select_window`

将指定窗口设为活跃窗口。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

### `select_layout`

应用窗口布局。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `window_index` | integer | ✅ | |
| `layout` | string | ✅ | `even-horizontal` / `even-vertical` / `main-horizontal` / `main-vertical` / `tiled` |

---

## 信息查询

### `pane_exists`

检查窗格是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |

**返回** `{"ok": true, "exists": true}`

### `pane_info`

获取窗格详细信息。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |

**返回** `{"ok": true, "info": {"pane_id": "%0", "window_id": "@0", "session_id": "$0", "index": 0, "size_cols": 170, "size_rows": 39, "command": null, "working_directory": "/root", "tags": []}}`

### `window_info`

获取窗口详细信息（名称、尺寸、索引）。需列出窗格请用 `list_window_panes`。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "info": {"window_id": "@0", "name": "ops", "size_cols": 170, "size_rows": 40, "index": 0}}`

### `list_window_panes`

列出窗口中所有窗格。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "panes": [{"pane_id": "%0", "active": true}]}`

---

## 生命周期

### `kill_session`

销毁整个会话（所有 window/pane/进程）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

**返回** `{"ok": true, "killed": true}`

---

## 进程控制

### `spawn_command`

在窗格中启动新进程（直接 exec，替换当前进程）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `command` | string | ✅ | 要执行的命令 |
| `args` | string[] | | 命令参数 |

**限制**：运行中的 pane 会拒绝（"pane still active"），需要先 `close_pane` 再 `session_create`。

### `shell_command`

通过 shell 执行命令（`/bin/sh -c`）。其余同 `spawn_command`。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `command` | string | ✅ | 要执行的 shell 命令 |

### `respawn_pane`

重新 spawn 窗格进程（默认选项）。用于 pane 中进程已退出或需要重置 shell 环境时。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | ✅ |

---

## 高级编排

### `broadcast_keys`

向同一会话中的多个 pane 同时发送相同按键。支持特殊键（同 `send_keys`）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_ids` | string[] | ✅ |
| `keys` | string | ✅ |

### `cmd_escape`

直接调用 rmux 命令行工具。当 bridge 协议未覆盖某些操作时使用。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `args` | string[] | ✅ |

**返回** `{"ok": true, "stdout": "...", "stderr": "", "exit_code": 0}`

---

## 文件传输

### `file_upload`

上传文件或目录到远程主机（通过 bridge TLS 通道）。目标目录不存在时自动创建。支持覆盖策略和 glob 排除。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `local_path` | string | ✅ | 本地文件路径 |
| `remote_path` | string | ✅ | 远程目标路径 |
| `overwrite` | string | | 覆盖策略: `overwrite`(默认) / `skip` / `rename` / `error` |
| `exclude` | string[] | | glob 排除模式，如 `["*.log", "target/**/*"]` |

**返回** `{"ok": true, "files": [...], "total": N, "uploaded": N, "skipped": N, "failed": 0}`

每个 file 对象：`{"path": "...", "status": "uploaded"|"skipped", "size": N, "sha256": "..."}`

> ⚠️ **AI Agent 使用约束**：除非用户明确指定，不要自作主张添加 `exclude` 或修改 `overwrite`。用户说传什么就传什么，不明白就二次确认。

### `file_download`

从远程主机下载文件（通过 bridge TLS 通道）。返回文件大小和 SHA256 校验值。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `remote_path` | string | ✅ | 远程文件路径 |
| `local_path` | string | ✅ | 本地保存路径 |

**返回** `{"ok": true, "file": {"uri": "...", "local_path": "...", "size": N, "sha256": "..."}}`

> ⚠️ **AI Agent 使用约束**：除非用户明确指定，不要添加额外过滤或修改路径。用户说下载什么就下载什么，不明白就二次确认。

---

## 操作审计

审计系统自动记录所有 MCP 工具调用到 SQLite 数据库，支持安全审计、运维排错、用量统计。

**数据库路径**：`~/.agent-ops/audit.db`（可通过 `--audit-db` 自定义）
**保留策略**：90 天 + 500MB 上限（先触发者生效）
**自动清理**：MCP Server 启动时后台每 10 分钟检查一次

审计数据通过 CLI 子命令查询，不依赖 MCP 协议：

### `audit query`

```bash
agent-ops-mcp audit query [--db <path>] [--host <host>] [--action <action>]
    [--agent <agent>] [--since <ISO8601>] [--until <ISO8601>]
    [--success <true|false>] [--limit <n>] [--format <table|json|jsonl>]
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `--db` | path | | 数据库路径，默认 `~/.agent-ops/audit.db` |
| `--host` | string | | 按主机名过滤 |
| `--action` | string | | 按操作类型过滤（Exec、FileUpload 等） |
| `--agent` | string | | 按 AI Agent 名称过滤 |
| `--since` | string | | 起始时间（ISO 8601） |
| `--until` | string | | 截止时间（ISO 8601） |
| `--success` | bool | | 按成功/失败过滤 |
| `--limit` | int | | 返回条数，默认 50 |
| `--format` | string | | 输出格式：`table`（默认）、`json`、`jsonl` |

### `audit stats`

```bash
agent-ops-mcp audit stats [--db <path>] [--since <ISO8601>]
```

输出总数、成功率、Top 主机/操作/Agent、平均耗时、最近失败。

### `audit cleanup`

```bash
agent-ops-mcp audit cleanup [--db <path>] [--older-than <days>] [--max-size <mb>]
```

手动触发清理。

### 审计事件字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | int | 自增主键 |
| `event_id` | uuid | 事件唯一 ID |
| `timestamp` | ISO 8601 | 操作时间（UTC） |
| `agent_name` | string | AI Agent 名称（来自 MCP initialize） |
| `host_name` | string | 目标主机 |
| `session_name` | string | 会话名 |
| `pane_id` | string | 窗格 ID（非 pane 操作为空） |
| `action` | string | 操作类型（33 种 AuditAction） |
| `detail` | string | 操作参数 |
| `output_summary` | string | Exec/CmdEscape 的输出摘要（前 500 字符） |
| `success` | bool | 操作是否成功 |
| `duration_ms` | int | 操作耗时（毫秒） |
| `error_message` | string | 失败时的错误信息 |

---

## 快速参考

| 场景 | 工具 |
|------|------|
| 跑命令看结果 | `exec` |
| 交互式程序 | `send_keys` + `capture_pane` |
| 等命令完成 | `wait_for_text` |
| 等进程退出 | `wait_exit` |
| 查看信息 | `pane_info` / `window_info` / `list_window_panes` |
| 多窗格分屏 | `split_pane` + `exec`（左右各跑不同命令） |
| 清理 | `close_pane` → `close_window` → `kill_session` |
| 特殊按键 | `send_keys`（`\x03`=Ctrl-C, `\n`=Enter） |
| 搜索 | `find_pane_text` |
| 审计查询 | `agent-ops-mcp audit query --host tf01 --action exec --format table` |
| 审计统计 | `agent-ops-mcp audit stats` |
