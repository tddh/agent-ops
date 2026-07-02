---
name: agent-ops-mcp
description: "使用 agent-ops MCP 工具操作远程主机的规范流程"
---

# agent-ops MCP 使用指南

## 强制规则

### 1. 默认会话名

**必须使用** `session_name="agent-ops"`，除非用户明确说"创建新会话"或指定其他名称。

- ❌ 禁止自作主张创建 `test-session`、`debug-session` 等
- ✅ 始终使用 `agent-ops` 作为默认会话

### 2. 默认 Pane

**必须使用** `%0`（agent-ops 会话的第一个 pane）。

- ✅ 通过 `list_window_panes` 确认实际 pane_id
- ❌ 不要假设 pane_id，每次都要验证

### 3. 操作流程

```
session_attach(host, session_name="agent-ops")
→ 如果不存在：session_create(host, session_name="agent-ops")
→ list_window_panes(host, session_name="agent-ops", window_index=0)
→ 使用返回的第一个 pane_id（通常是 %0）
```

### 4. 禁止行为

- ❌ 未经用户同意创建新会话
- ❌ 使用非 `agent-ops` 会话名（除非用户指定）
- ❌ 假设 pane_id 而不验证
- ❌ 执行完命令后主动清理 session

### 5. 会话生命周期

- ✅ **默认保留会话**：执行完命令后，不要主动关闭/清理 session
- ❌ 禁止调用 `kill_session`、`close_window`、`close_pane`（除非用户明确说"清理"、"关闭"、"销毁"）
- ✅ 用户可能需要查看执行结果或继续操作，保留会话是安全的默认行为

### 6. 以用户指令为主

用户的明确指令优先于以上所有默认规则。如果用户指令信息不明确，**必须先确认再执行**，禁止猜测。

**需要确认的场景：**
- ❓ 用户未指定主机 → "你要在哪台主机上操作？"
- ❓ 用户未指定操作目标 → "你要操作哪个文件/目录？"
- ❓ 用户说"清理一下"但未指定范围 → "你要清理哪些内容？"
- ❓ 用户指令有歧义 → 列出理解，让用户选择

**不需要确认的场景：**
- ✅ 用户明确说了主机名、会话名、命令等完整信息
- ✅ 上下文中已经明确（如刚才还在操作 tf01，用户说"再跑一次"）

## 工具使用示例

### ✅ 正确示例

```javascript
// 1. 检查会话是否存在
session_attach(host="tf01", session_name="agent-ops")

// 2. 如果不存在，创建会话
session_create(host="tf01", session_name="agent-ops")

// 3. 确认 pane_id
list_window_panes(host="tf01", session_name="agent-ops", window_index=0)

// 4. 使用确认的 pane_id 执行命令
exec(host="tf01", session_name="agent-ops", pane_id="%0", command="ls -la")
```

### ❌ 错误示例

```javascript
// 错误 1：自作主张创建新会话
session_create(host="tf01", session_name="test-session")  // ❌ 违反规则

// 错误 2：使用错误的会话名
exec(host="tf01", session_name="test-session", pane_id="%38", command="ls")  // ❌ 违反规则

// 错误 3：假设 pane_id
exec(host="tf01", session_name="agent-ops", pane_id="%0", command="ls")  // ❌ 未验证

// 错误 4：执行完主动清理
close_pane(host="tf01", session_name="agent-ops", pane_id="%0")  // ❌ 违反规则
```

## 常用工具速查

| 场景 | 工具 |
|------|------|
| 跑命令看结果 | `exec` |
| 交互式程序 | `send_keys` + `capture_pane` |
| 等命令完成 | `wait_for_text` |
| 等进程退出 | `wait_exit` |
| 查看信息 | `pane_info` / `window_info` / `list_window_panes` |
| 多窗格分屏 | `split_pane` + `exec` |
| 特殊按键 | `send_keys`（`\x03`=Ctrl-C, `\n`=Enter） |
| 搜索 | `find_pane_text` |
| 多机并发 | `batch_exec` / `batch_upload` / `batch_download` |

## 违反后果

违反以上规则 = BUG，必须立即修正。
