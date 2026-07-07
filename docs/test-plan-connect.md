# `agent-ops connect` 测试计划

> **版本**: v1.0 | **日期**: 2026-07-07 | **关联分支**: `feature/connect-interactive-terminal`

## 测试分层

```
┌─────────────────────────────────────┐
│         E2E 测试（手动）              │  CLI → Bridge → rmux 完整链路
├─────────────────────────────────────┤
│       集成测试（tokio test）          │  bridge 端流处理 + QUIC mock
├─────────────────────────────────────┤
│       单元测试（纯函数）              │  协议编解码、TLV 帧、状态机
└─────────────────────────────────────┘
```

---

## 一、单元测试

### 1.1 协议编解码 — `crates/rmux-bridge/src/interactive.rs`

| 测试 | 输入 | 预期 |
|------|------|------|
| `test_parse_attach_valid` | 完整合法的 Attach payload (session="test", pane="%0", 80x24, term="xterm") | 返回 `("test", "%0", 80, 24, "xterm")` |
| `test_parse_attach_empty_session` | session_name_len=0 的 payload | 返回 `Ok`，session_name 为空字符串 |
| `test_parse_attach_truncated` | 长度不足的 payload（offset 越界） | 返回 `Err`（panic substring） |
| `test_parse_attach_long_pane_id` | pane_id="%999"（4 字节） | 正确解析为 `"%999"` |
| `test_write_attached_empty_scrollback` | scrollback=&[] | 产出 `[0x81, 0x04,0x00, 0x00,0x00,0x00,0x00]` (7 bytes) |
| `test_write_attached_small_scrollback` | scrollback=b"hello" (5 bytes) | payload_len=9, scrollback_len=5, data="hello" |
| `test_write_attached_max_scrollback` | scrollback 长 65531 字节 | payload_len=65535 (u16::MAX)，不 panic |
| `test_write_error_encoding` | code=0x01, msg="test" | `[0x82, 0x08,0x00, 0x01, 0x04,0x00, "test"]` |
| `test_write_process_exited` | exit_code=42 | `[0x83, 0x04,0x00, 0x2A,0x00,0x00,0x00]` |

### 1.2 TLV 帧边界 — `crates/rmux-bridge/src/interactive.rs`

| 测试 | 输入 | 预期 |
|------|------|------|
| `test_read_u8` | stream 含 `[0x42]` | 返回 `0x42` |
| `test_read_u16_le` | stream 含 `[0x34, 0x12]` | 返回 `0x1234` |
| `test_read_bytes` | stream 含 `[0xAA, 0xBB, 0xCC]`, len=3 | 返回 `vec![0xAA, 0xBB, 0xCC]` |
| `test_read_bytes_truncated` | stream 只有 2 字节，len=5 | 返回 `Err` |

### 1.3 CLI 协议编解码 — `crates/agent-ops-cli/src/protocol.rs`

| 测试 | 输入 | 预期 |
|------|------|------|
| `test_write_attach_request` | session="s", pane="%0", 80x24 | 产出正确的 TLV 帧字节序列 |
| `test_read_attached_ok` | mock stream 含 `[0x81, len, scrollback_len, data]` | 返回 scrollback 数据 |
| `test_read_attached_error` | mock stream 含 `[0x82, len, 0x01, msg_len, "err"]` | 返回 `Err`，包含 "bridge error: err" |
| `test_read_attached_unexpected_type` | mock stream 含 `[0xFF, ...]` | 返回 `Err`，包含 "unexpected response type" |
| `test_write_resize` | 120x40 | `[0x02, 0x04,0x00, 0x78,0x00, 0x28,0x00]` |
| `test_write_detach` | — | `[0x03, 0x00,0x00]` |
| `test_send_json_frame` | `{"type":"test"}` | 产出 `[len_u32_le]["{\"type\":\"test\"}"]` |
| `test_recv_json_frame` | mock stream | 正确解析回 `serde_json::Value` |

---

## 二、集成测试

### 2.1 Bridge 端控制流 — `crates/rmux-bridge/tests/`

**前置条件**：需要有运行中的 rmux daemon（或 mock）。

| 测试 | 操作 | 验证点 |
|------|------|--------|
| `test_control_attach_ok` | 发送合法 Attach → 等待 Attached | 收到 0x81 + scrollback 数据 |
| `test_control_attach_session_not_found` | Attach 到不存在的 session | 收到 0x82 error code=0x01 |
| `test_control_attach_pane_not_found` | Attach 到不存在的 pane | 收到 0x82 error code=0x02 |
| `test_control_attach_bad_first_message` | 第一条消息不是 Attach (0x01) | 收到 0x82 error code=0x03 |
| `test_control_resize` | Attach 后发送 Resize | pane 尺寸变更，无错误 |
| `test_control_detach` | Attach 后发送 Detach | 连接正常关闭，session 仍存活 |
| `test_control_process_exited` | 数据流结束后检查控制流 | 收到 0x83 ProcessExited（通过 Notify 唤醒） |
| `test_control_unknown_message` | Attach 后发送未知 type | 不崩溃，日志 warn |

### 2.2 Bridge 端数据流 — `crates/rmux-bridge/tests/`

| 测试 | 操作 | 验证点 |
|------|------|--------|
| `test_data_forward_stdin_to_pty` | 控制流 Attach → 数据流发送 "echo hello\n" | PTY 输出包含 "hello" |
| `test_data_receive_pty_output` | 控制流 Attach → 数据流等待 | 收到 shell prompt 等 PTY 输出 |
| `test_data_timeout_waiting_control` | 只开数据流 (0x07)，不开控制流 (0x06) | 30 秒后 timeout error |
| `test_data_exit_code_written` | 数据流上执行 `exit 0` | session_state.exit_code = Some(0)，exit_notify 触发 |

### 2.3 QUIC 流分发 — `crates/rmux-bridge/src/files.rs`

| 测试 | 流类型 | 验证点 |
|------|--------|--------|
| `test_quic_dispatch_006` | 0x06 | 路由到 `handle_interactive_control` |
| `test_quic_dispatch_007` | 0x07 | 路由到 `handle_interactive_data` |
| `test_quic_dispatch_unknown` | 0xFF | 不崩溃，日志 warn |
| `test_quic_dispatch_001` | 0x01 | 路由到 JSON 协议帧（不变） |
| `test_quic_dispatch_002` | 0x02 | 路由到 upload（不变） |

### 2.4 ProtocolProxy 新方法 — `crates/rmux-bridge/src/protocol.rs`

| 测试 | 输入 | 预期 |
|------|------|--------|
| `test_get_session_exists` | 已存在的 session 名 | 返回 `Ok(Session)` |
| `test_get_session_not_found` | 不存在的 session 名 | 返回 `Err` |
| `test_get_pane_exists` | 有效的 (session, "%0") | 返回 `Ok(Pane)` |
| `test_get_pane_invalid_id` | 无效 pane_id "abc" | 返回 `Err`，含 "invalid pane_id" |

---

## 三、E2E 测试（手动）

### 3.1 基础连接

```
# 前置：bridge 运行中，session "agent-ops" 已创建
$ agent-ops connect localhost --session agent-ops --pane %0
```

| 检查点 | 预期 |
|--------|------|
| TLS 握手 | 连接建立 |
| AUTH 认证 | bridge 返回 OK |
| scrollback 回放 | 终端显示之前 AI Agent 执行的命令输出 |
| 输入 echo | 输入 `echo hello`，实时看到输出 |
| Ctrl+C | 中断当前命令 |
| 终端 resize | 调整终端窗口大小，vim/htop 正确 reflow |
| exit / Ctrl+D | 退出 connect，bridge 端 session 继续存活 |

### 3.2 错误场景

| 场景 | 命令 | 预期 |
|------|------|------|
| Session 不存在 | `connect localhost --session nosuch` | "session not found" 错误 |
| Pane 不存在 | `connect localhost --pane %99` | "pane not found" 错误 |
| 主机不存在 | `connect nosuch-host` | "host not found in hosts.yaml" |
| CA 证书错误 | `--ca-cert /bad/path` | "failed to read CA cert" |
| Bridge 未运行 | 正常连接 | "failed to connect to bridge" |
| Token 错误 | hosts.yaml 中 token 错误 | "bridge auth failed" |

### 3.3 `agent-ops list`

```
$ agent-ops list localhost
```

| 检查点 | 预期 |
|--------|------|
| 有 session | 列出 session 名称和数量 |
| 无 session | "No active sessions" |

### 3.4 `--insecure` 模式

```
$ agent-ops --insecure connect localhost
```

| 检查点 | 预期 |
|--------|------|
| 跳过 TLS 验证 | 连接成功（即使证书不匹配） |

### 3.5 `--readonly` 模式

```
$ agent-ops connect localhost --readonly
```

| 检查点 | 预期 |
|--------|------|
| 看到输出 | 能看到 PTY 输出 |
| 不能输入 | 键盘输入不会发送到远程 pane |

### 3.6 断线重连（Phase 3）

| 场景 | 预期 |
|------|------|
| 网络断开后恢复 | CLI 检测断线，提示用户重连 |
| 重新 connect 同一 pane | 看到断线期间的输出（Ghost Buffer） |

---

## 四、压力 / 边界测试

| 测试 | 操作 | 预期 |
|------|------|--------|
| 大输出 | `find /` 或 `cat /dev/urandom` | 不崩溃，输出可控 |
| 高频 resize | 快速拖动终端窗口 | resize 事件不丢失，pane 尺寸最终正确 |
| 长时间空闲 | connect 后 30 分钟不操作 | 连接保持，不发 ProcessExited |
| 并发 connect | 两个 CLI 同时 connect 同一 pane | Phase 3 验证多人共享 |
| 二进制输出 | `cat /dev/urandom \| head` | 不乱码，不崩溃 |
| scrollback 超限 | pane 历史超过 64KB | 只回放最近 ~64KB |

---

## 五、实现优先级

| 优先级 | 测试类别 | 预估工时 |
|--------|---------|---------|
| P0 | 单元测试：协议编解码 (1.1-1.3) | 2h |
| P0 | 集成测试：控制流基本 (2.1) | 2h |
| P1 | 集成测试：数据流 (2.2) | 1.5h |
| P1 | E2E：基础连接 + 错误场景 (3.1-3.2) | 1.5h |
| P2 | 集成测试：流分发 + ProtocolProxy (2.3-2.4) | 1h |
| P2 | E2E：高级场景 (3.3-3.6) | 1h |
| P3 | 压力/边界测试 (4) | 1h |

**总计约 10 工时**。P0 可在 CI 中自动运行，P1/P2 需要 rmux daemon 环境。
