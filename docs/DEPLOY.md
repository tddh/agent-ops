# agent-ops 部署文档

> 最后更新：2026-06-30

## 架构

```
                               QUIC/TCP :9778  终端操作 + 文件传输
┌─────────────────┐  MCP stdio  ┌──────────────┐ ════════════════════════╗ ┌──────────────────┐   Unix Socket  ┌─────────┐
│  AI 客户端        │◄─────────►│ agent-ops-mcp │                       ║ │   rmux-bridge     │◄─────────────►│  RMUX   │
│ (OpenCode/Claude) │            │  (macOS/Linux/Windows) │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ║ │   (Linux 远程主机)  │                │ daemon  │
└─────────────────┘            └──────────────┘ ════════════════════════╝ └──────────────────┘                └─────────┘
```

- **agent-ops-mcp**: MCP Server，运行在 AI 客户端同机，提供 42 个终端控制工具 + 操作审计 CLI
- **rmux-bridge**: 部署在每台目标 Linux 主机上，TLS 加密代理 → RMUX daemon。终端操作与文件传输统一走 QUIC/TCP 双协议，共享 9778 端口（QUIC 优先，UDP 不可用时自动降级 TCP/TLS）
- **RMUX daemon**: 每个 Linux 主机上的终端多路复用器

## 前置条件

| 组件 | 要求 |
|------|------|
| 目标主机 | Linux x86_64，systemd，有 SSH 访问 |
| RMUX | `rmux` daemon 已安装并运行（`curl -fsSL https://rmux.io/install.sh \| sh`） |
| 构建机 | Rust 1.85+，`x86_64-linux-musl-gcc`（交叉编译用 `brew install FiloSottile/musl-cross/musl-cross`） |
| 端口 | bridge 监听 9778（TLS/QUIC） |
| 证书 | 自签名 TLS 证书（`openssl` 即可） |

## 快速开始

### 1. 构建

```bash
# 本机构建（macOS 开发）
cargo build -p agent-ops-mcp --release

# 交叉编译 bridge（Linux x86_64，静态链接）
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --target x86_64-unknown-linux-musl --release -p rmux-bridge

# 或用 just 快捷命令
just release-linux          # 交叉编译 bridge + mcp
just build-mcp              # 本机构建 mcp
```

构建产物：
- `target/release/agent-ops-mcp` — MCP server（本地运行）
- `target/x86_64-unknown-linux-musl/release/rmux-bridge` — bridge（部署到远程）

### 2. 部署 bridge

`just deploy` 一键完成远程部署，**自动生成 TLS 证书**，无需手动操作。

**本地测试**（`just certs`）：
```bash
just certs
# → certs/bridge.crt（CN=rmux-bridge, SAN=localhost/127.0.0.1）
# 仅用于本机调试，MCP server 和 bridge 都在同一台机器
```

**远程部署**（`just deploy`）：
```bash
# 一键部署：生成证书 → 上传二进制 → 配置 systemd → 启动服务
just deploy host=root@<your-bridge-ip> token=<your-token>

# 或手动：
BRIDGE_TOKEN="<your-token>" bash deploy/install-bridge.sh \
  ./target/x86_64-unknown-linux-musl/release/rmux-bridge \
  root@<your-bridge-ip>
```

部署脚本自动完成：
- 在**远程主机上**用 openssl 生成 TLS 证书（CN/SAN 设为目标主机 IP）
- 上传 `rmux-bridge` 二进制到 `/opt/agent-ops/`
- 写入 token 到 `/opt/agent-ops/bridge.env`（权限 600）
- 创建 `rmux-bridge.service`（`systemctl enable --now`）

远程证书路径：`/opt/agent-ops/certs/bridge.{crt,key}`
MCP server 需要用远程的 `bridge.crt`（或 CA 根证书 `ca.crt`）作为 `--ca-cert`。

> ⚠️ 两套证书**不能混用**：本地测试用 `just certs` 生成的自签名证书；远程部署用 `just deploy`（通过 `install-bridge.sh`）自动签发独立主机证书。

**其他 Justfile 命令：**

| 命令 | 说明 |
|------|------|
| `just certs` | 生成本地测试用自签名证书 |
| `just certs-host host=<name>` | 为指定主机生成 TLS 证书 |
| `just run-bridge host=<name>` | 本地启动 bridge（开发/测试） |

**生成的 systemd 服务文件**（`/etc/systemd/system/rmux-bridge.service`）：

```ini
[Unit]
Description=RMUX Bridge - TCP/TLS to Unix socket proxy
After=network.target

[Service]
Type=simple
EnvironmentFile=/opt/agent-ops/bridge.env
ExecStart=/opt/agent-ops/rmux-bridge \
    --listen-addr 0.0.0.0:9778 \
    --quic-listen-addr 0.0.0.0:9778 \
    --max-connections 256 \
    --rmux-socket /tmp/rmux-1000/default \
    --tls-cert /opt/agent-ops/certs/bridge.crt \
    --tls-key /opt/agent-ops/certs/bridge.key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### 3. 更新 bridge（已有安装）

```bash
# 交叉编译
just release-linux

# 替换二进制 + 重启
ssh root@<your-bridge-ip> "systemctl stop rmux-bridge"
scp target/x86_64-unknown-linux-musl/release/rmux-bridge root@<your-bridge-ip>:/opt/agent-ops/
ssh root@<your-bridge-ip> "systemctl start rmux-bridge"

# 验证
ssh root@<your-bridge-ip> "systemctl status rmux-bridge --no-pager"
```

### 4. Bridge CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--listen-addr` | `0.0.0.0:9778` | TCP/TLS 监听地址（终端操作） |
| `--quic-listen-addr` | `0.0.0.0:9778` | QUIC/UDP 监听地址（终端操作 + 文件传输） |
| `--max-connections` | `256` | 最大并发连接数，0=无限制（`MAX_CONNECTIONS` 环境变量） |
| `--rmux-socket` | `/tmp/rmux-1000/default` | RMUX daemon Unix socket 路径 |
| `--tls-cert` | `certs/<host>.crt` | TLS 证书路径（CA 签发） |
| `--tls-key` | `certs/<host>.key` | TLS 私钥路径 |
| `--auth-token` | 环境变量 `BRIDGE_AUTH_TOKEN` | 认证令牌 |
| `--log-level` | `info` | 日志级别：trace/debug/info/warn/error（`RUST_LOG` 环境变量） |

> **QUIC/TCP 共享 9778 端口**：MCP client 优先使用 QUIC，UDP 被防火墙阻断时自动降级 TCP/TLS。

### 5. MCP Server CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--hosts-file` | `config/hosts.yaml` | 主机注册表路径 |
| `--ca-cert` | 无 | CA 证书路径（不传则拒绝连接） |
| `--insecure` | 无 | 跳过 TLS 证书验证（仅调试） |
| `--audit-db` | `~/.agent-ops/audit.db` | 审计数据库路径 |
| `--audit-retention-days` | `90` | 审计数据保留天数 |
| `--audit-max-size-mb` | `500` | 审计数据库大小上限 (MB) |
| `--audit-cleanup-interval-secs` | `600` | 自动清理间隔（秒） |

### 6. 认证模式

Bridge 支持两种 token 格式：

| 格式 | 示例 | 说明 |
|------|------|------|
| 静态 token | `<your-token>` | 常数时间比较，防时序攻击 |
| JWT (HS256) | `jwt:eyJhbG...` | 前缀 `jwt:` 触发 JWT 验证，支持过期时间 (`exp`) |

在 `config/hosts.yaml` 的 `bridge_token` 和系统环境变量 `BRIDGE_AUTH_TOKEN` 中统一使用同一种格式。

### 7. 配置主机注册表

创建 `config/hosts.yaml`：

```yaml
hosts:
  - name: tf01                         # MCP 工具中引用的主机名
    bridge_addr: <your-bridge-ip>:9778     # bridge 地址
    bridge_token: "<your-token>"              # 认证 token
    group: production                  # 分组（host_filter 用）
    tags: [web, nginx]                  # 标签
    labels:                             # 键值对标签
      dc: shanghai
      rack: a3
```

### 8. 配置 MCP Server（OpenCode）

编辑 `~/.config/opencode/opencode.json`：

```json
{
  "mcp": {
    "agent-ops": {
      "type": "local",
      "command": ["/path/to/agent-ops/target/release/agent-ops-mcp"],
      "args": [
        "--hosts-file",
        "/path/to/agent-ops/config/hosts.test.yaml",
        "--ca-cert",
        "/tmp/bridge-remote.crt"
      ],
      "enabled": true
    }
  }
}
```

### 9. 验证

```bash
# 直接调 MCP 测试
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"host_list","arguments":{}}}' \
  | target/release/agent-ops-mcp --hosts-file config/hosts.test.yaml --ca-cert /tmp/bridge-remote.crt 2>/dev/null
```

信任首次连接：将远程 bridge 的 `bridge.crt` 复制到本地，通过 `--ca-cert` 参数指定。自签名证书 `SkipVerification` 模式也通过代码硬编码支持（调试用，不建议生产）。

## 运维

```bash
# 查看 bridge 状态
ssh root@<your-bridge-ip> "systemctl status rmux-bridge"

# 查看日志
ssh root@<your-bridge-ip> "journalctl -u rmux-bridge -f"

# 检查 RMUX socket 是否存在
ssh root@<your-bridge-ip> "ls -la /tmp/rmux-*/default"
```

### 审计查询

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

## 目录结构

```
~/.agent-ops/                      # MCP Server 本地
├── audit.db                       # 审计数据库（SQLite）

/opt/agent-ops/                   # 远程主机
├── rmux-bridge                   # bridge 二进制
├── bridge.env                    # BRIDGE_AUTH_TOKEN（权限 600）
└── certs/
    ├── bridge.crt                # TLS 证书
    └── bridge.key                # TLS 私钥（权限 600）

/etc/systemd/system/
└── rmux-bridge.service          # systemd 服务文件
```

## 故障排查

| 症状 | 检查 |
|------|------|
| MCP 工具返回 `connection refused` | `systemctl status rmux-bridge`，确认 bridge 在运行 |
| `authentication failed` | 检查 `bridge.env` 中的 `BRIDGE_AUTH_TOKEN` 与 `hosts.yaml` 中 `bridge_token` 是否一致 |
| TLS 握手失败 | `--ca-cert` 指向的证书是否与 bridge 端一致；或确认代码中启用了 `SkipVerification` |
| `unknown request type` | bridge 版本过旧，重新交叉编译部署 |
| RMUX socket 找不到 | `ls /tmp/rmux-*/default`，确认 rmux daemon 在运行（socket 路径取决于运行用户 UID：root → `/tmp/rmux-0/default`，普通用户 → `/tmp/rmux-1000/default`，部署脚本自动检测实际路径） |

## 安全

### Unix Socket

rmux daemon 按运行用户 UID 创建 Unix socket：
- root → `/tmp/rmux-0/default`
- 普通用户（UID 1000） → `/tmp/rmux-1000/default`

部署脚本自动检测实际路径，无需手动指定。Socket 权限为 `srw-------`（仅 owner 可读写），其他用户无法访问。

如果需要在非 `/tmp` 路径运行（避免系统 tmp 清理、或满足合规要求），配置 rmux daemon 使用自定义 socket 路径后，同步更新 bridge 的 `--rmux-socket` 参数。

### TLS 安全模式

有三种模式：

| 模式 | 触发条件 | 安全等级 |
|------|---------|:---:|
| CA 验证 | `--ca-cert /path/to/ca.crt` | ✅ 验证服务器身份，防中间人 |
| 跳过验证 | `--insecure` flag | ⚠️ 加密但不验证身份（仅调试用） |
| 拒绝连接 | 既无 CA 又无 --insecure | 🔒 默认行为 |

### 自签名证书

**生产环境建议**：自建 CA，为每台 bridge 签发证书，MCP server 只持有 CA 根证书。

```bash
# 生成 CA
openssl req -x509 -newkey rsa:4096 -keyout ca.key -out ca.crt -days 3650 -nodes \
  -subj "/CN=agent-ops-ca" -addext "basicConstraints=critical,CA:TRUE"

# 为 bridge 签发（替换 <your-bridge-ip> 为实际 IP）
openssl req -new -newkey rsa:2048 -keyout bridge.key -out bridge.csr -nodes \
  -subj "/CN=<your-bridge-ip>" -addext "subjectAltName=DNS:<your-bridge-ip>,IP:<your-bridge-ip>"
openssl x509 -req -in bridge.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out bridge.crt -days 365

# MCP server 启动时指定 CA
agent-ops-mcp --ca-cert ca.crt ...
```

