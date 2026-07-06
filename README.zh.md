# agent-ops

> AI Agent 远程操作 Linux 主机的安全基础设施 —— 基于 rmux 提供持久化终端会话与全链路审计日志，通过 MCP 协议对接所有主流 AI 客户端，支持文件传输与多主机编排。

[English](README.md)

## 为什么需要 agent-ops？

AI Agent 的推理和工具调用能力已经足够强，正在从「帮你生成命令」走向「自主接管终端执行任务」——部署服务、排查故障、跑编译训练长任务，全程无需人工介入。但传统终端工具（SSH、tmux）从设计之初就是给人用的交互工具，不是给程序调用的编程接口。agent-ops 基于 **rmux** 构建，把终端会话从人机交互界面变成了程序可调度的资源，在这个基础之上补了三层面向生产的封装。

生产环境落地有三个绕不开的问题，现有工具几乎都没有系统性解决：

- **可靠性**：纯 SSH 方案断连即进程终止，长任务极易失败；传统 tmux 自动化靠 `send-keys + sleep + grep`，时序偏移就会出错。
- **可审计**：企业让 AI 操作服务器，必须追溯「什么时间、哪台机器、执行了什么命令、结果如何」。纯 SSH 工具大多没有内置审计能力。
- **安全边界**：直接把 SSH 密钥交给 AI 客户端风险极高。agent-ops 通过 Bridge 代理 + Token 认证 + TLS 加密，将服务器权限收敛在目标主机本地，AI 端不直接持有服务器密钥。

这三层分别是：**协议层**（MCP 标准接口对接所有主流 Agent）、**管理层**（多主机注册、分组标签、批量广播操作）、**合规层**（全链路结构化 SQLite 审计日志），补上了 AI Agent 从原型到生产落地的基础设施缺口。

## 架构

```mermaid
graph LR
    A[AI 客户端] <-->|MCP stdio| B[agent-ops-mcp<br/>macOS/Linux/Windows]
    B <-->|QUIC/TCP :9778<br/>终端操作 + 文件传输| C[rmux-bridge<br/>Linux 远程主机]
    C <-->|Unix Socket| D[RMUX daemon<br/>基于 rmux]
```

- **agent-ops-mcp** — MCP Server，运行在 AI 客户端同机，提供 61 个终端控制工具 + 操作审计 CLI
- **rmux-bridge** — 部署在每台目标 Linux 主机上的 TLS 加密代理，将 JSON 请求翻译为 RMUX daemon 调用
- **RMUX daemon** — 每台 Linux 主机上的终端多路复用器（基于 rmux）

**依赖关系：**

| 组件 | 运行位置 | 依赖 |
|------|---------|------|
| `agent-ops-mcp` | AI 客户端（macOS/Linux/Windows） | 无 — 单个二进制即可 |
| `rmux-bridge` | 每台目标 Linux 主机 | **RMUX daemon**（`curl -fsSL https://rmux.io/install.sh \| sh`） |
| RMUX daemon | 每台目标 Linux 主机 | rmux（需要安装） |

> 💡 部署时 bridge 会自动检测 RMUX socket 路径，无需手动配置。

## 核心能力

| 能力 | 说明 |
|------|------|
| **交互式会话管理** | 创建/销毁/列举会话，多窗格分屏，窗口布局 |
| **命令执行** | `exec` 一站式执行（sentinel 检测 + exit code 提取），支持交互式程序（send_keys + capture_pane） |
| **输出等待** | `wait_for_text` 等待终端出现指定文本，`wait_exit` 等待进程退出 |
| **文件传输** | QUIC 通道上传/下载，支持目录递归上传和下载 + 并发 |
| **端口转发** | 通过 QUIC 隧道访问远程内网服务（数据库、API 等） |
| **多主机编排** | 主机注册表 + 分组/标签/模式过滤，broadcast_keys 多窗格广播 |
| **操作审计** | SQLite 审计日志，每次工具调用自动记录，支持 CLI 查询/统计/清理 |

## 快速开始

### 构建

```bash
# 本机构建（macOS 开发）
cargo build -p agent-ops-mcp --release

# 交叉编译 bridge（Linux x86_64，静态链接）
just release-linux
```

### 部署

```bash
# 步骤 1：部署 rmux daemon（远程主机）
bash deploy/install-daemon.sh root@<your-bridge-ip>

# 步骤 2：编译并部署 bridge（一键）
just release-linux
just deploy host=root@<your-bridge-ip> token=<your-token>
```

### 配置主机注册表

创建 `config/hosts.yaml`（参考 `config/hosts.example.yaml`）：

```yaml
hosts:
  - name: prod-web-01
    bridge_addr: 10.0.1.10:9778
    bridge_token: "your-token-here"
    group: production
    tags: [web, nginx]
    labels:
      dc: shanghai
```

### 配置 MCP Server

编辑 `~/.config/opencode/opencode.json`（参考 `config/mcp-config.example.json`）：

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

> 远程部署使用 `ca.crt`；本地自签名测试可用 `bridge.crt`。
> 调试可用 `--insecure` 跳过证书验证（不推荐生产环境使用）。

## 安全

| 模式 | 触发条件 | 安全等级 |
|------|---------|:---:|
| CA 验证 | `--ca-cert /path/to/ca.crt` | ✅ 验证服务器身份，防中间人 |
| 跳过验证 | `--insecure` flag | ⚠️ 加密但不验证身份（仅调试用） |
| 拒绝连接 | 既无 CA 又无 --insecure | 🔒 默认行为 |

**生产环境建议**：自建 CA，为每台 bridge 签发证书，MCP server 只持有 CA 根证书。

## 审计查询

```bash
# 查最近操作
agent-ops-mcp audit query --format table

# 查特定主机的命令执行记录
agent-ops-mcp audit query --host tf01 --action exec --since 2026-06-01

# 统计概览
agent-ops-mcp audit stats

# 手动清理
agent-ops-mcp audit cleanup --older-than 30
```

审计数据默认存储在 `~/.agent-ops/audit.db`，保留 90 天，上限 500MB。

## 工具列表

共 61 个 MCP 工具，覆盖完整终端生命周期：

| 类别 | 工具 |
|------|------|
| 主机管理 | `host_list`, `host_filter` |
| 会话管理 | `session_create`, `session_list`, `session_attach`, `session_detach`, `kill_session` |
| 终端输入 | `send_keys`, `send_text`, `broadcast_keys` |
| 终端输出 | `capture_pane`, `capture_region`, `wait_for_text`, `wait_for_bytes`, `find_pane_text`, `find_text_all`, `stream_pane` |
| 命令执行 | `exec`, `wait_exit`, `wait_stable`, `collect_until_exit`, `spawn_command`, `shell_command`, `respawn_pane`, `cmd_escape` |
| 窗格操作 | `split_pane`, `split_pane_with`, `break_pane`, `join_pane`, `swap_pane`, `resize_pane`, `set_pane_title`, `get_pane_title`, `clear_history`, `close_pane`, `pane_info`, `pane_exists` |
| 窗口操作 | `split_window`, `close_window`, `rename_window`, `resize_window`, `select_window`, `select_layout`, `window_info`, `list_window_panes` |
| 发现与查询 | `find_panes`, `find_sessions`, `get_pane_by_title`, `host_capabilities` |
| 粘贴板 | `list_buffers`, `paste_buffer`, `delete_buffer` |
| 文件传输 | `file_upload`, `file_download` |
| 批量操作 | `batch_exec`, `batch_upload`, `batch_download` |
| 端口转发 | `tunnel_create`, `tunnel_list`, `tunnel_close` |
| 部署升级 | `deploy_bridge` |

> 💡 `stream_pane` 适用于长命令实时输出监控（阻塞读，增量返回），替代 capture_pane 轮询。

完整工具文档见 [docs/TOOLS.md](docs/TOOLS.md)。

## 开发

```bash
just check       # cargo check --workspace
just test        # cargo test --workspace
just fmt         # cargo fmt --all
just lint        # cargo clippy --workspace -- -D warnings
just build       # cargo build --workspace
```

## 技术栈

- **语言**：Rust 1.85+（edition 2021）
- **异步运行时**：tokio
- **TLS**：rustls（无 openssl 依赖）
- **终端多路复用**：rmux-sdk
- **审计存储**：rusqlite（bundled SQLite）
- **MCP 传输**：stdio（JSON-RPC 2.0）

## 文档

- [工具文档](docs/TOOLS.md) — 61 个 MCP 工具的完整参数与返回值
- [部署文档](docs/DEPLOY.md) — 架构、构建、部署、运维、安全
- [贡献指南](CONTRIBUTING.md)
- [安全策略](SECURITY.md)
- [更新日志](CHANGELOG.md)

## License

MIT
