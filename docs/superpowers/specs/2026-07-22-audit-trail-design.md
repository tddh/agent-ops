# agent-ops 操作审计追踪设计

## 概述

为 agent-ops 补全操作审计追踪能力，实现「谁在什么时间做了什么操作」的全覆盖记录，事后可追溯、可举证。

**核心原则：** Bridge 是审计数据的唯一产生点。所有录制和事件日志在 bridge 侧完成，MCP/CLI 只是触发者。

**部署模型：**
```
用户本地: MCP server (stdio) + CLI (TUI)
远程主机: rmux-bridge (per host)
```

## 现状

- MCP 侧：已有完整 SQLite 审计（~60 种 AuditAction，支持查询/统计/清理）
- CLI 侧：零审计，纯 PTY 透传，无任何记录
- Bridge 侧：无审计，无录制（`.sisyphus/plans/bridge-connect-audit.md` 已规划未实现）

## 设计范围

| 方向 | 内容 |
|------|------|
| CLI 会话录制 | Bridge 侧全量 PTY 录制（asciinema v2） |
| Bridge 事件日志 | 连接/认证/文件/tunnel 等事件写入本地 SQLite |
| 管理操作审计 | MCP 侧新增 5 个 audit action（审计查询/清理/配置重载本身也被记录） |
| 集中拉取 | MCP 定期从 bridge 拉取录制文件到本地（便利副本，便于查询回放） |

---

## Phase 1a：Bridge PTY 全量录制

### 录制点

在 `rmux-bridge/src/interactive.rs` 的 PTY passthrough 循环中插入录制层：

```
client ←→ QUIC stream 0x07 ←→ [CastRecorder] ←→ master_fd (openpty) ←→ rmux attach
                                     │
                                     ▼
                           recordings/{date}/{file}.cast
```

### 录制格式：asciinema v2 (JSONL)

```jsonl
{"version": 2, "width": 120, "height": 40, "timestamp": 1721600000, "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}}
[0.000000, "o", "user@host:~$ "]
[0.523411, "i", "ls -la\r"]
[0.530122, "o", "total 48\r\ndrwxr-xr-x ..."]
[1.200000, "o", "user@host:~$ "]
```

- `"o"` = output（master_fd → client）
- `"i"` = input（client → master_fd）
- 时间戳为相对录制开始的秒数（monotonic clock）

### 文件命名与目录

```
{recording_dir}/
├── 2026-07-22/
│   ├── agent-ops_%0_1721600000_a3f8.cast
│   ├── agent-ops_%0_1721600000_a3f8.meta
│   └── agent-ops_%1_1721600123_b7c2.cast
└── 2026-07-21/
    └── ...
```

- 按日期分目录，便于按天清理
- 文件名：`{session}_{pane}_{epoch}_{client_id前4字节}.cast`
- 默认 `recording_dir = {bridge所在目录}/recordings`（自包含部署，无需额外创建系统目录）

### 写入策略

- **Append-only**：每条事件立即 `write()`，定期 `fsync()`（每 5 秒或每 64KB，先到先触发）
- **非阻塞**：录制写入在独立 tokio task，通过 bounded channel（容量 4096）接收事件；channel 满时丢弃最老的 output 事件并记录 `[gap]` 标记，绝不阻塞 PTY 数据流
- **异常退出**：bridge 被 kill -9 时最多丢失最后一个 fsync 周期（≤5秒）；正常 detach/exit 时写 `["exit", code]` 事件并关闭文件

### 文件完整性

- 录制文件关闭时计算 sha256，写入 `.meta` sidecar 文件
- `.meta` 内容：`{"sha256": "...", "synced": false, "closed_at": "...", "duration_secs": N, "size_bytes": N}`
- 关闭后对 `.cast` 文件设置 `chattr +a`（append-only 文件系统属性），防止非 root 篡改
- 清理时需要先 `chattr -a` 再删除（bridge 以 root 或专用用户运行）

### 配置项（bridge 新增）

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `recording_enabled` | `true` | 总开关 |
| `recording_dir` | `{bridge所在目录}/recordings` | 录制文件目录 |
| `recording_retention_days` | `90` | 保留天数 |
| `recording_max_size_mb` | `500` | 单主机录制总容量上限 |
| `recording_fsync_interval_secs` | `5` | fsync 间隔 |

### 清理逻辑

bridge 启动时 + 每小时执行一次：
1. 删除超过 `retention_days` 的日期目录（先 `chattr -a` 再删）
2. 总大小超过 `max_size_mb` 时，从最老日期目录开始删除
3. 已标记 `synced: true` 且超期的文件优先删除
4. 清理操作写入 bridge 事件日志

---

## Phase 1b：Bridge 连接事件日志

### 存储

Bridge 本地 SQLite：`{bridge所在目录}/bridge_events.db`，WAL 模式。

### 表结构

```sql
CREATE TABLE connect_events (
    event_id      TEXT PRIMARY KEY,
    timestamp     TEXT NOT NULL,
    client_addr   TEXT NOT NULL,
    client_id     TEXT,
    auth_method   TEXT NOT NULL DEFAULT 'token',
    event_type    TEXT NOT NULL,
    session_name  TEXT,
    pane_id       TEXT,
    cols          INTEGER,
    rows          INTEGER,
    detail        TEXT,
    duration_secs REAL,
    exit_code     INTEGER
);

CREATE INDEX idx_events_ts ON connect_events(timestamp);
CREATE INDEX idx_events_type ON connect_events(event_type, timestamp);
CREATE INDEX idx_events_session ON connect_events(session_name, timestamp);
```

### 事件类型

| event_type | 触发时机 | detail 内容 |
|-----------|---------|------------|
| `auth_success` | AUTH 帧验证通过 | `{"agent_name": "..."}` 或 `{"mode": "cli"}` |
| `auth_failure` | token 不匹配 | `{"token_len": N}`（不记录 token 本身） |
| `attach` | PTY stream 0x07 建立 | `{"recording_file": "..."}` |
| `detach` | 客户端主动断开 stream | `{"duration_secs": N}` |
| `exit` | rmux attach 子进程退出 | `{"exit_code": N, "duration_secs": N}` |
| `session_create` | JSON 命令创建 session | `{"session_name": "..."}` |
| `session_kill` | JSON 命令销毁 session | `{"session_name": "..."}` |
| `file_upload` | 文件上传完成 | `{"path": "...", "size_bytes": N}` |
| `file_download` | 文件下载完成 | `{"path": "...", "size_bytes": N}` |
| `tunnel_open` | tunnel stream 建立 | `{"target": "127.0.0.1:8080"}` |
| `tunnel_close` | tunnel 关闭 | `{"duration_secs": N}` |
| `config_reload` | SIGHUP / 配置热加载 | `{"hosts_changed": N}` |
| `recording_cleanup` | 清理任务执行 | `{"files_deleted": N, "bytes_freed": N}` |

### 写入方式

- `tokio::task::spawn_blocking` + rusqlite（复用 MCP 侧模式）
- 写入失败 `tracing::error!` 降级，不阻塞主逻辑
- 共享 `agent-ops-core` 中的类型定义（扩展 `AuditAction` 枚举）

### 保留策略

90 天 + 50MB 容量上限。清理与录制清理合并到同一定时任务。

### 查询接口

通过 bridge JSON control stream（0x06）暴露：

```json
{"command": "audit_query", "params": {"event_type": "auth_failure", "since": "2026-07-01"}}
{"command": "audit_stats", "params": {"since": "2026-07-01"}}
```

---

## Phase 1c：MCP 管理操作审计补强

MCP 侧 `AuditAction` 枚举新增：

| 新 action | 触发时机 |
|-----------|---------|
| `audit_query` | 查询审计日志 |
| `audit_stats` | 查看审计统计 |
| `audit_cleanup` | 手动触发清理 |
| `config_reload` | SIGHUP 热加载 hosts.yaml |
| `bridge_audit_query` | 通过 MCP 查询 bridge 侧事件 |

形成闭环：查看/清理审计的操作本身也被审计。

---

## Phase 1d：MCP `query_bridge_audit` tool

新增 MCP tool，通过 QUIC 连接 bridge 的 control stream（0x06）转发查询：

```json
{
  "name": "query_bridge_audit",
  "description": "查询目标主机 bridge 侧的连接事件日志",
  "inputSchema": {
    "host": "string (required)",
    "event_type": "string (optional)",
    "session_name": "string (optional)",
    "since": "string (optional, RFC3339)",
    "until": "string (optional, RFC3339)",
    "limit": "integer (optional, default 50)"
  }
}
```

---

## Phase 2a：Bridge 录制文件同步接口

Bridge control stream（0x06）新增命令：

```json
{"command": "list_unsynced_recordings"}
→ {"files": [{"file": "...", "date": "...", "size_bytes": N, "sha256": "..."}]}

{"command": "mark_synced", "params": {"file": "agent-ops_%0_1721600000_a3f8.cast"}}
→ {"ok": true}
```

- `list_unsynced_recordings`：扫描 recording_dir，返回所有 `.meta` 中 `synced == false` 的文件
- `mark_synced`：更新 `.meta` 中 `synced` 为 `true`

---

## Phase 2b：MCP 定期拉取

### 连接方向

```
MCP（定时任务）──QUIC──→ Bridge（0x06 查询 + 0x03 download 拉文件）
```

始终 MCP 主动连 bridge，bridge 无需知道 MCP 地址。

### 流程

1. MCP 定时任务（默认 300 秒间隔）遍历 hosts.yaml 所有主机
2. 连接 bridge → control stream 发送 `list_unsynced_recordings`
3. 对每个未同步文件，开 download stream（0x03）拉取
4. 写入 `~/.agent-ops/recordings/{host_name}/{date}/{file}.cast`
5. 校验 sha256
6. 成功后通过 control stream 发送 `mark_synced`

### MCP 侧配置

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--audit-sync-interval-secs` | `300` | 拉取间隔 |
| `--recordings-dir` | `~/.agent-ops/recordings` | 本地存储目录 |
| `--recordings-retention-days` | `90` | 本地保留天数 |
| `--recordings-max-size-mb` | `5000` | 本地容量上限 |

### 容错

- 单主机连接失败跳过，下轮重试
- 大文件复用现有 download stream 的 chunk 机制
- MCP 重启后从 bridge 侧 `.meta` 状态恢复，不重复拉取

---

## Phase 2c：MCP 录制查询 tool

| Tool | 说明 |
|------|------|
| `list_recordings` | 按 host/date/session 查询已拉取的录制文件列表 |
| `get_recording` | 返回录制文件路径或内容，访问本身被审计（`audit_query` action） |

---

## 安全加固

### 防篡改

| 层面 | 措施 |
|------|------|
| 文件系统权限 | 录制目录 `0700`，文件 `0400`，owner = bridge 运行用户 |
| `chattr +a` | 录制文件关闭后设置 append-only 属性，非 root 无法修改/删除 |
| 路径隔离 | 录制目录不在 MCP file 工具可访问的 base_path 内 |
| 完整性校验 | sha256 sidecar，Phase 2 拉取时校验 |
| 操作关联 | 录制文件含 client_id（QUIC 连接标识），可关联到具体连接来源 |

### 防信息泄露

- `auth_failure` 事件不记录 token 内容，只记录长度
- 录制文件通过权限 + 目录隔离保护
- `get_recording` tool 访问被审计

### 防拒绝服务

- 录制 channel 有界（4096），满时丢弃 output 不阻塞 PTY
- 目录容量上限防磁盘打满
- 清理任务定期执行

### 录制加密（暂不实施，预留接口）

**结论：Phase 1 不加密。**

评估依据：
- 当前部署场景（内部运维、bridge 专用用户运行），`0700` + `chattr +a` + 路径隔离已足够
- 加密的核心价值（防 root / 防文件外泄）在当前场景不是主要威胁
- 密钥在本机则 root 可解密，加密形同虚设；密钥在远端则增加故障点
- 密钥丢失 = 所有录制永久不可读，运维风险大于安全收益
- asciicast v2 加密后无法直接用 `asciinema play` 回放

**后续如需加密（等保/合规驱动）：**
- bridge 配置 `recording_encryption_key`（32 字节 hex，环境变量注入）
- AES-256-CTR 加密 .cast 文件内容
- MCP `get_recording` tool 透明解密后返回
- 代码层面预留：CastRecorder 写入通过 trait 抽象，后续插入加密层不改调用方

---

## 实施分期

| Phase | 内容 | 改动范围 |
|-------|------|---------|
| **1a** | Bridge CastRecorder（PTY 全量录制） | `rmux-bridge` |
| **1b** | Bridge connect_events SQLite | `rmux-bridge` + `agent-ops-core` |
| **1c** | MCP 新增 5 个管理操作 audit action | `agent-ops-mcp` |
| **1d** | MCP `query_bridge_audit` tool | `agent-ops-mcp` |
| **2a** | Bridge `list_unsynced_recordings` / `mark_synced` | `rmux-bridge` |
| **2b** | MCP 定时拉取 + 本地存储 + 容量管理 | `agent-ops-mcp` |
| **2c** | MCP `list_recordings` / `get_recording` tool | `agent-ops-mcp` |

Phase 1 完成后：CLI 全量录制 + bridge 事件日志 + 管理操作闭环审计。
Phase 2 完成后：集中存储便利副本 + 查询回放能力。

---

## 社区参考

| 工具 | 借鉴点 |
|------|--------|
| Teleport | 代理层录制 + 两层存储（本地→中心） |
| Boundary | BSR→asciicast 导出；我们直接用 asciicast v2 作原生格式 |
| K8s audit | 结构化事件 + 原始流分离 |
| sudo 1.9+ | 远程日志服务器 append-only 模型 |
| pam_tty_audit | `chattr +a` / 内核不可篡改模式 |

## 已知限制（当前接受）

- MCP `agent_name` 由客户端自报，无密码学验证（依赖 stdio 信任模型）
- MCP 侧审计 SQLite 无防篡改保护（root 可改）
- MCP 本地拉取的录制为便利副本，用户可删除（bridge 侧原始文件为审计证据）
- 审计写入失败静默降级（tracing::error），可能丢失个别记录
