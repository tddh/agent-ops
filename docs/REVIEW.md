# agent-ops 全面审查报告

> 审查日期：2026-07-02 | 范围：代码功能 + 文档完整性 + 安全健壮性

---

## 🔴 P0 — 必须修复

### 1. `host_filter` 工具缺少审计调用

**问题**：`tools.rs:79-106`，`host_filter` 执行过滤后直接返回，无任何 `audit()` 调用。33 个 MCP 工具中唯一完全缺失审计的（除 stream_pane 外）。

**修复**：在 `tools.rs` 的 `host_filter` 函数返回前添加审计调用，并在 `AuditAction` 枚举中新增 `HostFilter` 变体。

---

### 2. `stream_pane` 工具缺少审计调用

**问题**：`tools.rs:293-302`，`stream_pane` 直接返回 `recv_json_frame` 结果，无审计。

**修复**：添加审计调用，在 `AuditAction` 中新增 `StreamSubscribe` 变体。

---

### 3. `split_pane` 使用错误的 `AuditAction` 变体

**问题**：`tools.rs:584` 使用 `AuditAction::SplitWindow` 记录 pane 分割操作。`AuditAction` 枚举中不存在 `SplitPane` 变体。

**修复**：在 `types.rs` 的 `AuditAction` 枚举中添加 `SplitPane`，将 `tools.rs:584` 改为 `AuditAction::SplitPane`。

---

### 4. QUIC 流分发碰撞 Bug

**问题**：`files.rs:251-253`，bridge 端 `handle_quic_stream` 读取 QUIC 流的第一个字节判断文件上传/下载还是 JSON 协议。当 JSON 帧长度 `len % 256 == 2` 或 `len % 256 == 3` 时，LE32 的第一字节恰好是 `0x02` 或 `0x03`，被错误路由到文件处理器。发生概率约 1/128。

**修复**：在 JSON 协议流前添加一个专用魔数字节（如 `0x00`），或将文件流类型字节改为使用不可打印字符避免与 LE32 长度前缀冲突。

---

### 5. `--insecure` 标志文档存在但 CLI 代码缺失

**问题**：READM/DEPLOY 均文档化了 `--insecure` 标志，但 `main.rs` 的 `Cli` 结构体无此参数。`transport.rs` 实现了 `SkipVerification` 逻辑，但所有调用点硬编码 `insecure: false`。

**修复**：在 `main.rs` 添加 `#[arg(long)] insecure: bool`，在 `ToolContext` 中传递该值到 `connect_to_bridge_hybrid`。

---

### 6. README 中 `--ca-cert` 示例路径错误

**问题**：中英文 README 的 MCP 配置示例用 `"--ca-cert", "/path/to/bridge.crt"`。远程部署（`just deploy`）使用 CA 签发模型，MCP Server 应使用 `ca.crt`。`bridge.crt` 仅在本地自签名测试时有效。

**修复**：区分两种场景，远程部署示例改为 `ca.crt`，或添加注释说明区别。

---

### 7. MCP 工具计数不一致

**问题**：README 声明"35 个 MCP 工具"，代码实际有 36 个（含 `stream_pane`）。READM 工具表中也遗漏了 `stream_pane`。

**修复**：将计数改为 36，或明确标注（*35 个可用 + 1 个不可用*），并在表中补充 `stream_pane`。

---

## 🟠 P1 — 应当修复

### 8. `session_create` 默认 session_name 不一致

**问题**：代码 `tools.rs:110` 默认 `"agent-ops"`，TOOLS.md 第 50 行声称默认 `"agent-session"`。

**修复**：统一为 `"agent-ops"`（与项目名一致），更新 TOOLS.md。

---

### 9. `upload_dir` 并发失败时结果被丢弃

**问题**：`files.rs:174-181`，并发上传失败的 entry 仅 `tracing::warn!` 后跳过，不添加到 results。返回的 `total`/`failed` 计数不准确。

**修复**：将失败的 entry 以 `status: "failed"` 添加到 results。

---

### 10. `audit stats --since` 参数未使用

**问题**：`audit/query.rs:127`，`stats` 函数接收 `_since` 参数但未传递给 SQL 查询。无论传入什么时间范围，stats 统计全量数据。

**修复**：在 SQL 查询中添加 `WHERE timestamp >= ?` 条件。

---

### 11. `exec` 输出过滤过于激进

**问题**：`tools.rs:484-486`，过滤所有 `[` 开头 `]` 结尾且长度 ≤8 的行。合法输出如 `[OK]`、`[WARN]`、`[1/4]` 会被误删。

**修复**：使用更严格的过滤条件，如匹配 UUID 前缀 `[a-f0-9]{3}` 格式。

---

### 12. `exec` 轮询间隔固定 200ms

**问题**：`tools.rs:518`，快速命令（毫秒级）多等 200ms，高频轮询浪费带宽。

**修复**：使用自适应间隔或指数退避。

---

### 13. CHANGELOG.md 严重过时

**问题**：仅记录初始版本，缺失所有后续特性（QUIC 优先连接、CA 证书体系、Windows 支持、连接限制等）。

**修复**：补充 v0.1.0 完整变更记录。

---

### 14. `OverwriteMode::Rename` 和 `NoClobber` 未在 Bridge 端实现

**问题**：Bridge `files.rs` 的 `handle_upload_quic` 只检查 `mode == 0x02`（Skip），其他模式均视为 Overwrite。`Rename`(0x03) 和 `NoClobber`(0x04) 行为等同于 Overwrite，可能静默覆盖文件。

**修复**：在 bridge 端实现完整的 OverwriteMode 处理：Rename 添加后缀，NoClobber 已存在时返回错误。

---

## 🟡 P2 — 建议修复

### 15. `drain_file_data` 死代码

**问题**：`rmux-bridge/src/files.rs:231`，函数从未被调用，编译器产生 `dead_code` 警告。

**修复**：删除或补充调用逻辑。

---

### 16. MCP 错误处理模式不一致

**问题**：工具操作失败时返回 `Ok(json!({"ok": false, ...}))` 而非 `Err(...)`。MCP 客户端在 JSON-RPC 层面收到成功响应，错误信息需二次解析 `result.content[0].text`。

**修复**：让关键失败返回 `Err()` 触发 JSON-RPC error，或使用 MCP 2024-11-05 的 `"isError": true` 标记。

---

### 17. 无连接池复用 ⏸️ 暂不实施

**问题**：每个 MCP 工具调用都执行完整的 TLS 握手 + Token 认证后发送单个请求并断开。

**状态**：当前短连接模式简单可靠，每连接独立状态无副作用。连接池引入复杂度（连接生命周期管理、故障恢复），收益不明显（QUIC 0-RTT 已极大降低重连成本）。**暂不实施**。

**修复**：—

---

### 18. `MAX_FRAME_SIZE` 重复定义

**问题**：`tools.rs:384` 和 `proxy.rs:17` 各自定义 `const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024`。一端修改可能遗漏另一端。

**修复**：提取到 `agent-ops-core` 共享。

---

### 19. 数值类型截断无溢出检查

**问题**：`tools.rs:526, 641-642`，`cols`/`rows`/`width`/`height` 使用 `as u16` 转换，传入 >65535 值时静默截断。

**修复**：使用 `u64::try_from()` 或验证范围后返回错误。

---

### 20. `collect_files` 递归无深度限制

**问题**：`files.rs:249`，深层目录树或符号链接循环可能导致栈溢出。

**修复**：添加最大递归深度或改用迭代遍历。

---

### 21. `stream_subscribe` Bridge 端 task 泄漏

**问题**：`tools.rs:300-301`，客户端 `stream_pane` 只读一条响应后断开连接。Bridge 端已 spawn 的 streaming task 持续尝试写入已关闭的连接，每次调用泄漏一个 task + channel。

**修复**：在 bridge 端监听 writer 关闭事件并提前终止 task。

---

### 22. DEPLOY.md Unix Socket 路径自相矛盾

**问题**：第 262 行说默认 `/tmp/rmux-1000/default`，第 267 行说 `/tmp/rmux-0/default`。两者分别对应 UID 1000 和 root，文档未解释此差异。

**修复**：统一说明为"取决于运行用户 UID，root 为 `/tmp/rmux-0/default`，普通用户为 `/tmp/rmux-{uid}/default`"。

---

### 23. `--max-connections` 未在 DEPLOY.md 文档中列出

**问题**：Bridge CLI 已有 `--max-connections` 参数（默认 256），但 DEPLOY.md 的 CLI 参数参考表未收录。

**修复**：在表中添加该参数条目。

---

### 24. `CONTRIBUTING.md` GitHub URL 含 `<org>` 占位符

**问题**：`CONTRIBUTING.md:12`，`https://github.com/<org>/agent-ops.git` 中 `<org>` 未替换为实际值。

**修复**：改为实际仓库路径。

---

### 25. Bridge 端 TCP 文件传输协议无人使用

**问题**：`proxy.rs` 中有 `FILE_UPLOAD_FRAME`/`FILE_DOWNLOAD_FRAME` 的 TCP inline 二进制流处理，以及 `files.rs` 中的 TCP yamux 流处理。MCP 客户端仅使用 QUIC 传输文件，这两套 TCP 协议是死代码。

**修复**：评估是否保留作为后备方案。若不需要则移除。

---

### 26. `EnsureSessionPolicy::CreateOrReuse` 状态污染风险

**问题**：`protocol.rs:80`，session 已存在时复用旧 session，可能带有之前的 pane 状态和残留进程。

**修复**：考虑使用 `CreateOnly` 或文档化行为。

---

## 🟢 P3 — 已跟踪

| # | 问题 | 位置 |
|---|------|------|
| 27 | README.zh.md JSON 示例含中文注释（非法 JSON） | `README.zh.md:100` |
| 28 | DEPLOY.md cert 流程说明略显冗余（`just deploy` 已自动处理） | `DEPLOY.md:52-64` |
| 29 | `just certs-host`、`run-bridge` 等命令未在 README/DEPLOY 中提及 | justfile |
| 30 | `mcp-config.example.json` 缺少审计相关参数 | `config/` |
| 31 | systemd 模板未显式设置 `--quic-listen-addr` 和 `--max-connections` | `install-bridge.sh` |
| 32 | 健康检查端口计算 `+1` 可能溢出 u16 或对 IPv6 失效 | `main.rs:53` |
| 33 | `cmd_escape` 发送冗余 `host` 字段到 bridge | `tools.rs:232` |
| 34 | `drain_file_data` 和 batch upload handler (`0x04`) 为隐藏/未使用功能 | bridge files.rs |
| 35 | `audit output_summary` 切片在 UTF-8 字符边界可能 panic | `tools.rs:496` |

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

---

## 修复建议优先级

1. **P0 代码缺陷**（#1-#4, #6-#7）：审计缺失、协议碰撞、文档错误 — 直接影响功能正确性和用户体验
2. **P0 CLI 补全**（#5）：`--insecure` 标志 — 文档承诺但不可用的功能
3. **P1 功能缺陷**（#8-#14）：默认值不一致、输出过滤、错误处理、CHANGELOG
4. **P2 架构改进**（#15-#26）：死代码清理、错误处理统一、连接池、溢出检查
5. **P3 打磨**（#27-#35）：文档细节、占位符、冗余字段
