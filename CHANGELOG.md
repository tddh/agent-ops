# Changelog

## [0.4.1] — 2026-07-20

### Changed
- **升级 rmux-sdk 0.8→0.9**：wire v3→v5，需 daemon 0.9+ 配套

### Fixed
- **CLI 连接后按键无效**：rmux 0.9 将 `allow-passthrough` 默认改为 `off`，bridge PTY attach 时按键被 daemon 拦截。新增 `config/rmux.conf` 模板，部署时写入 `set -g allow-passthrough on`
- **Windows CLI 编译**：`#[cfg(unix)]` 条件编译包裹 AI 面板的 stderr 抑制逻辑

### Added
- `config/rmux.conf` daemon 配置模板（mouse、history-limit、allow-passthrough）
- Release 包包含 `rmux.conf`

## [0.3.0] — 2026-07-15

### Changed
- **代码组织重构**：大文件拆分，提升可维护性
  - `tools.rs` (3661行) → `tools/` 目录 12 个子模块
  - `protocol.rs` (1965行) → `protocol/` 目录 7 个子模块
  - `main.rs` (1163行) → `handler.rs` + `audit_cli.rs` + `schema.rs`
  - `router.rs` `RwLock::unwrap()` 改为 `expect()`，防止 poisoned lock panic
- **Exec 输出不再过滤 prompt 行**：`exec` 返回的 `output` 现在包含 start_marker → sentinel 区间的完整终端上下文（含 shell 提示符、命令回显），不做行级过滤——所见即所得。旧版本会过滤掉提示符和命令回显行
- **Exec 等待机制从轮询改为事件驱动**：命令发送后通过 bridge 端 `wait_for_text` 阻塞等待 sentinel 标记出现，替代旧的 MCP 侧轮询 `capture_pane` 方式，降低网络开销和延迟
- **Exec 默认超时从 30s 增加到 600s（10 分钟）**：适应编译、包安装等较长时间命令，仅作为安全兜底。正常命令无需手动设置 `timeout_ms`

### Added
- **配置热加载**：支持在不重启 MCP Server 的情况下重新加载 `hosts.yaml` 配置
  - 新增 `reload_config` MCP 工具，AI Agent 可主动触发
  - 支持 SIGHUP 信号触发（`kill -HUP <pid>`），运维友好
  - 加载失败时保留原有配置，不影响运行中服务

### Security
- **文件路径穿越防护**：Bridge 端 `file_upload`/`file_download` 拒绝包含 `..` 的路径，防止路径穿越攻击
- **下载目录遍历防护**：MCP 端验证远端返回的相对路径不含 `..` 且非绝对路径
- **隧道目标白名单（SSRF 防护）**：`hosts.yaml` 新增可选 `allowed_tunnel_targets` 字段，支持 glob 模式限制端口转发目标（如 `"127.0.0.1:5432"`、`"10.0.1.*:*"`），不配置则全部允许（向后兼容）

### Added
- **交互式终端直连**：新增 `agent-ops-cli` crate，提供 `agent-ops-cli connect` CLI 命令
  - PTY + `rmux attach-session` 子进程透传方案，完美支持 vim/htop 等 TUI 程序
  - QUIC 双流协议（0x06 控制 + 0x07 数据），控制面与数据面分离
  - crossterm raw mode 终端转发，支持 resize/detach
- 部署脚本拆分：`install-daemon.sh`（rmux daemon）+ `install-bridge.sh`（rmux-bridge），职责独立
- `/etc/profile.d/agent-ops.sh`：自动设置 `RMUX_TMPDIR` 环境变量，用户登录后可直接 `rmux a -t agent-ops`
- Bridge 请求级别日志：INFO 显示请求摘要（type/session/duration），DEBUG 显示完整请求/响应 JSON
- Bridge `--log-level` 参数（默认 `info`，支持 trace/debug/info/warn/error，可通过 `RUST_LOG` 环境变量覆盖）
- 端口转发功能：`tunnel_create`、`tunnel_list`、`tunnel_close` 三个 MCP 工具
  - 通过 QUIC 隧道访问远程内网服务（数据库、API 等）
  - 1 小时空闲超时 + 15 秒 keepalive，适合长连接场景
  - 64KB 缓冲区，支持 TCP 半关闭处理
  - 完整审计日志记录
- **终端状态感知**：新增 `terminal_state.rs` 模块，`TerminalState` 枚举覆盖 8 种终端状态（ready/running/password/confirm/repl/editor/pager/unknown）
  - 5 个工具（`capture_pane`、`exec`、`wait_for_text`、`wait_stable`、`pane_info`）新增 `terminal_state` 和 `cursor` 返回字段
  - 24 个单元测试覆盖启发式检测逻辑
- **Exec 执行前安全检查**：`exec` 执行命令前检测终端状态，非 `ready` 状态自动拒绝执行
  - 新增响应字段：`pre_terminal_state`（执行前状态）和 `refused`（是否被拒绝）
  - 防止命令注入到 vim/less/密码提示/REPL 等非 shell 上下文
  - 向后兼容：检测失败时正常执行，不影响已有用法

### Removed
- **移除 TCP/TLS 回退传输**：MCP 与 Bridge 之间仅使用 QUIC 协议，删除 TCP listener、yamux 多路复用、`proxy_legacy`、`BridgeStream::Tcp` 等约 700 行代码。移除 `tokio-rustls`（MCP 侧）和 `tokio-yamux`（Bridge 侧）依赖
- `--insecure` 参数完全禁用，`--ca-cert` 改为必填（H-03 高危安全风险：消除 MITM 攻击面）

### Changed
- **rmux-sdk 从 0.7 升级到 0.8**（wire protocol v3，需同步升级 daemon）
- 移除 JWT 认证支持，认证简化为纯静态 token 常量时间比较
- 工作空间依赖统一到根 Cargo.toml，13 个共享依赖版本集中管理
- `rmux-bridge.service` 添加 `After=rmux-daemon.service` 和 `Requires=rmux-daemon.service`
- 部署脚本 socket 检测路径增加 `$HOME/.rmux`（与 daemon service 的 `RMUX_TMPDIR` 保持一致）
- Bridge CLI `--rmux-socket` 默认值从 `/tmp/rmux-1000/default` 改为动态检测

### Security
- host_filter 通配符过滤从手写正则改为 `glob::Pattern`，消除 ReDoS 风险
- Exec 执行前终端状态检查：防止在 vim/less/密码提示/REPL 等非 shell 上下文中注入命令，避免意外数据修改或信息泄露

## [0.1.0] — 2026-07-02

### Added
- 39 MCP 工具（38 可用 + 1 开发中 `stream_pane`）
- 3 个批量操作工具：`batch_exec`、`batch_upload`、`batch_download`（多主机并发执行/上传/下载）
- QUIC 协议传输（UDP :9778）
- CA 签发 + 按主机独立证书的多主机 PKI 体系
- Windows/macOS/Linux 客户端原生支持
- Bridge 并发连接限制（`--max-connections`，默认 256）
- Token 认证，恒定时间比较
- SQLite 审计日志（query/stats/cleanup）
- 主机注册表（group/tag/label 过滤、broadcast_keys）
- 文件传输：QUIC 上传/下载、目录递归并发上传
- systemd 服务部署 + `just deploy` 一键部署
- `--insecure` 标志用于调试环境跳过 TLS 验证
- 审计 CLI 子命令（`audit query/stats/cleanup`）

### Fixed
- 生产代码 `unwrap()` 改为 poison-safe 模式
- Bridge QUIC handler 支持 JSON RPC 终端操作
