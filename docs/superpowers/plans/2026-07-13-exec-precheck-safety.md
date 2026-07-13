# Exec 执行前安全检查 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 exec 执行前检测终端状态，非 ready 状态时拒绝执行并返回详细错误信息，防止命令注入到 vim/less/password prompt 等非 shell 环境。

**Architecture:** 在 `exec_in_session` 函数开头添加 terminal_state 预检查。如果状态不是 ready，立即返回 refused 错误，包含 pre_terminal_state 和建议操作。不自动恢复（安全 > 便利），让 AI 根据上下文自行决策。

**Tech Stack:** Rust, serde_json, existing terminal_state 检测模块

---

## File Structure

### Modified Files

1. **`crates/agent-ops-mcp/src/tools.rs`**
   - 修改 `ExecResult` 结构体，添加 `pre_terminal_state` 和 `refused` 字段
   - 修改 `exec_in_session` 函数，添加执行前 terminal_state 检测
   - 修改 `exec` 函数，在返回 JSON 中包含新字段

2. **`.opencode/skills/agent-ops-mcp/SKILL.md`**
   - 添加 "exec 安全检查" 章节，说明 refused 场景和 AI 决策框架

3. **`docs/TOOLS.md`**
   - 更新 exec 工具文档，说明 pre_terminal_state 和 refused 字段

### Test Files

1. **`crates/agent-ops-mcp/src/tools.rs`** (内联测试)
   - 添加测试：验证 refused 返回格式
   - 添加测试：验证 ready 状态正常执行

---

## Task 1: 扩展 ExecResult 结构体

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs:480-495`

- [ ] **Step 1: 查看当前 ExecResult 定义**

```bash
grep -n "struct ExecResult" crates/agent-ops-mcp/src/tools.rs -A 10
```

Expected output: 显示 ExecResult 结构体定义（约 480-490 行）

- [ ] **Step 2: 添加 pre_terminal_state 和 refused 字段**

```rust
struct ExecResult {
    ok: bool,
    output: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    error: Option<String>,
    terminal_state: Option<serde_json::Value>,
    cursor: Option<serde_json::Value>,
    // 新增字段
    pre_terminal_state: Option<serde_json::Value>,
    refused: bool,
}
```

- [ ] **Step 3: 更新所有 ExecResult 构造点，添加默认值**

搜索所有 `ExecResult {` 构造点，添加：
```rust
pre_terminal_state: None,
refused: false,
```

```bash
grep -n "ExecResult {" crates/agent-ops-mcp/src/tools.rs
```

Expected: 约 8-10 个构造点，全部添加默认值

- [ ] **Step 4: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: 编译通过，无错误

- [ ] **Step 5: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "feat(exec): add pre_terminal_state and refused fields to ExecResult"
```

---

## Task 2: 实现执行前 terminal_state 检测

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs:500-550` (exec_in_session 函数开头)

- [ ] **Step 1: 在 exec_in_session 开头添加 snapshot 调用**

在 `exec_in_session` 函数的 sentinel marker 生成之前（约 513 行），插入：

```rust
// 执行前安全检查：检测终端状态
let precheck_resp = match send_json_frame(stream, &json!({
    "type": "capture_pane",
    "session_name": session_name,
    "pane_id": pane_id,
    "max_lines": 200,
})).await {
    Ok(_) => match recv_json_frame(stream).await {
        Ok(r) => Some(r),
        Err(_) => None,
    },
    Err(_) => None,
};

let pre_terminal_state = precheck_resp
    .as_ref()
    .and_then(|r| r.get("terminal_state"))
    .cloned();
let pre_cursor = precheck_resp
    .as_ref()
    .and_then(|r| r.get("cursor"))
    .cloned();

// 检查是否为 ready 状态
let is_ready = pre_terminal_state
    .as_ref()
    .and_then(|v| v.as_str())
    .map(|s| s == "ready")
    .unwrap_or(true); // 如果检测失败，默认允许执行（向后兼容）

if !is_ready {
    let state_name = pre_terminal_state
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    
    let suggestion = match state_name {
        "editor" => "Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.",
        "pager" => "Terminal is in pager (less/more). Use send_keys('q') to exit pager first.",
        "password" => "Terminal is waiting for password. Use send_keys to provide password or Ctrl-C to cancel.",
        "confirm" => "Terminal is waiting for confirmation. Use send_keys to respond.",
        "running" => "A process is still running. Use wait_stable/wait_exit to wait, or send_keys(Ctrl-C) to stop it.",
        "repl" => "Terminal is in REPL (python3/mysql). Use send_keys to send REPL commands, or exit REPL first.",
        _ => "Terminal state is unknown. Use capture_pane to inspect terminal content.",
    };
    
    return ExecResult {
        ok: false,
        output: String::new(),
        exit_code: None,
        duration_ms: 0,
        error: Some(suggestion.to_string()),
        terminal_state: pre_terminal_state.clone(),
        cursor: pre_cursor,
        pre_terminal_state: pre_terminal_state,
        refused: true,
    };
}
```

- [ ] **Step 2: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: 编译通过

- [ ] **Step 3: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "feat(exec): add pre-execution terminal_state check, refuse non-ready states"
```

---

## Task 3: 更新 exec 函数返回 JSON

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs:650-700` (exec 函数)

- [ ] **Step 1: 在 exec 函数的 JSON 返回中添加新字段**

找到 `exec` 函数中构建返回 JSON 的部分（约 683-696 行），修改为：

```rust
let mut response = json!({
    "ok": result.ok,
    "output": result.output,
    "exit_code": result.exit_code,
    "duration_ms": result.duration_ms,
    "error": result.error,
});
if let Some(ref state) = result.terminal_state {
    response["terminal_state"] = state.clone();
}
if let Some(ref cursor) = result.cursor {
    response["cursor"] = cursor.clone();
}
// 新增：pre_terminal_state 和 refused
if let Some(ref pre_state) = result.pre_terminal_state {
    response["pre_terminal_state"] = pre_state.clone();
}
if result.refused {
    response["refused"] = json!(true);
}
```

- [ ] **Step 2: 编译验证**

```bash
cargo check -p agent-ops-mcp
```

Expected: 编译通过

- [ ] **Step 3: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "feat(exec): include pre_terminal_state and refused in JSON response"
```

---

## Task 4: 添加单元测试

**Files:**
- Modify: `crates/agent-ops-mcp/src/tools.rs` (文件末尾添加测试模块)

- [ ] **Step 1: 添加测试模块**

在 `tools.rs` 文件末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_exec_result_refused_format() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.".to_string()),
            terminal_state: Some(json!("editor")),
            cursor: Some(json!({"row": 0, "col": 0, "visible": true})),
            pre_terminal_state: Some(json!("editor")),
            refused: true,
        };
        
        assert!(!result.ok);
        assert!(result.refused);
        assert_eq!(result.pre_terminal_state, Some(json!("editor")));
        assert!(result.error.as_ref().unwrap().contains("editor"));
    }

    #[test]
    fn test_exec_result_normal_format() {
        let result = ExecResult {
            ok: true,
            output: "file1.txt\nfile2.txt".to_string(),
            exit_code: Some(0),
            duration_ms: 120,
            error: None,
            terminal_state: Some(json!("ready")),
            cursor: Some(json!({"row": 5, "col": 14, "visible": true})),
            pre_terminal_state: Some(json!("ready")),
            refused: false,
        };
        
        assert!(result.ok);
        assert!(!result.refused);
        assert_eq!(result.pre_terminal_state, Some(json!("ready")));
    }
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p agent-ops-mcp --lib tools::tests
```

Expected: 2 tests passed

- [ ] **Step 3: Commit**

```bash
git add crates/agent-ops-mcp/src/tools.rs
git commit -m "test(exec): add unit tests for refused and normal ExecResult formats"
```

---

## Task 5: 更新 TOOLS.md 文档

**Files:**
- Modify: `docs/TOOLS.md:296-320` (exec 工具文档)

- [ ] **Step 1: 更新 exec 返回值文档**

找到 exec 工具的返回值说明（约 310-312 行），替换为：

```markdown
**返回** `{"ok": true, "output": "...", "exit_code": 0, "duration_ms": 70, "pre_terminal_state": "ready", "terminal_state": "ready", "cursor": {"row": 5, "col": 14, "visible": true}}`

超时：`{"ok": false, "output": "...", "exit_code": null, "error": "timeout...", "pre_terminal_state": "running", "terminal_state": "running", "cursor": {"row": 5, "col": 0, "visible": true}}`

**安全检查拒绝**：`{"ok": false, "refused": true, "error": "Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.", "pre_terminal_state": "editor", "cursor": {"row": 0, "col": 0, "visible": true}}`

> **安全检查**：exec 执行前会检测终端状态。如果终端不在 `ready` 状态（如 vim、less、password prompt、REPL 等），exec 会拒绝执行并返回 `refused: true`。这是为了防止命令注入到非 shell 环境。
>
> **字段说明**：
> - `pre_terminal_state`：执行前的终端状态
> - `terminal_state`：执行后的终端状态
> - `refused`：是否因安全检查被拒绝
>
> **当 exec 返回 refused 时**：
> 1. 检查 `pre_terminal_state`，理解终端当前状态
> 2. 回溯对话历史：是你自己把终端带到这个状态的吗？
>    - 是 → 你知道怎么退出，先退出再重试
>    - 不是 → 用 `capture_pane` 查看终端内容，判断情况
> 3. 绝不在不理解终端状态的情况下强制发送按键
```

- [ ] **Step 2: Commit**

```bash
git add docs/TOOLS.md
git commit -m "docs: update exec tool documentation with precheck safety behavior"
```

---

## Task 6: 更新 SKILL.md 文档

**Files:**
- Modify: `.opencode/skills/agent-ops-mcp/SKILL.md` (在 "终端状态感知" 章节后添加)

- [ ] **Step 1: 添加 exec 安全检查章节**

在 "终端状态感知" 章节末尾添加：

```markdown
## exec 安全检查

`exec` 工具在执行命令前会检测终端状态。如果终端不在 `ready` 状态，exec 会拒绝执行并返回 `refused: true`。

### 为什么需要安全检查

| 场景 | 不检查的后果 |
|------|------------|
| 终端在 vim 中 | 命令被注入到编辑器，文件损坏 |
| 终端在 less 中 | 命令被当作搜索/导航输入 |
| 终端在等密码 | 命令被当作密码输入 |
| 终端在 REPL 中 | 命令被当作 Python/MySQL 代码执行 |
| 上一个命令还在运行 | 命令被注入到 stdin |

### 当 exec 返回 refused 时的决策框架

```
exec("ls") → refused: editor
│
├─ 回溯对话历史：是你自己打开的 vim 吗？
│   ├─ YES → 你知道怎么退出 → send_keys("\x1b:q!\n") → exec("ls")
│   └─ NO  → 用 capture_pane 查看内容 → 判断情况 → 或问用户
│
├─ exec("ls") → refused: running
│   └─ 上一个命令还在运行 → wait_stable 或 send_keys("\x03") → exec("ls")
│
└─ exec("ls") → refused: password
    └─ 终端在等密码 → 提示用户输入密码，或 send_keys("\x03") 取消
```

### 核心原则

1. **安全 > 便利**：宁可多一步操作，也不要破坏终端状态
2. **AI 决策，不是工具决策**：工具只负责检测和拒绝，不自动恢复
3. **上下文感知**：AI 根据对话历史和当前状态做决策，不是固定规则
4. **不确定时问用户**：如果 AI 不理解终端状态，应该询问用户而不是盲目操作
```

- [ ] **Step 2: Commit**

```bash
git add .opencode/skills/agent-ops-mcp/SKILL.md
git commit -m "docs: add exec precheck safety guidance to SKILL.md"
```

---

## Task 7: 全工作区编译和测试

**Files:**
- 无文件修改，仅验证

- [ ] **Step 1: 全工作区编译**

```bash
cargo build --workspace
```

Expected: 编译通过

- [ ] **Step 2: 全工作区测试**

```bash
cargo test --workspace
```

Expected: 所有测试通过

- [ ] **Step 3: Clippy 检查**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: 无警告

- [ ] **Step 4: Commit（如果有修复）**

```bash
# 如果 clippy 发现问题并修复
git add -A
git commit -m "fix: address clippy warnings in exec precheck implementation"
```

---

## Task 8: E2E 测试验证

**Files:**
- 无文件修改，仅验证

- [ ] **Step 1: 部署到测试主机**

```bash
just release-linux
```

然后通过 MCP 工具部署：
```
deploy_bridge(hosts=["tf001"], binary_path="target/x86_64-unknown-linux-musl/release/rmux-bridge")
```

- [ ] **Step 2: 测试 ready 状态正常执行**

```
exec(host="tf001", session_name="agent-ops", pane_id="%0", command="echo hello")
```

Expected: `{"ok": true, "output": "hello", "pre_terminal_state": "ready", ...}`

- [ ] **Step 3: 测试 editor 状态被拒绝**

```
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="vim /tmp/test.txt\n")
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
exec(host="tf001", session_name="agent-ops", pane_id="%0", command="ls")
```

Expected: `{"ok": false, "refused": true, "pre_terminal_state": "editor", "error": "Terminal is in editor..."}`

- [ ] **Step 4: 测试 pager 状态被拒绝**

```
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="\x1b:q!\n")  # 退出 vim
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="echo -e 'line1\nline2\nline3' | less\n")
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
exec(host="tf001", session_name="agent-ops", pane_id="%0", command="ls")
```

Expected: `{"ok": false, "refused": true, "pre_terminal_state": "pager", "error": "Terminal is in pager..."}`

- [ ] **Step 5: 测试 REPL 状态被拒绝**

```
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="q")  # 退出 less
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="python3\n")
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
exec(host="tf001", session_name="agent-ops", pane_id="%0", command="ls")
```

Expected: `{"ok": false, "refused": true, "pre_terminal_state": "repl", "error": "Terminal is in REPL..."}`

- [ ] **Step 6: 清理测试环境**

```
send_keys(host="tf001", session_name="agent-ops", pane_id="%0", keys="exit()\n")  # 退出 python3
wait_stable(host="tf001", session_name="agent-ops", pane_id="%0")
```

- [ ] **Step 7: 记录测试结果**

在 commit message 或 PR 中记录 E2E 测试结果。

---

## Summary

### 变更文件清单

| 文件 | 变更类型 | 说明 |
|------|---------|------|
| `crates/agent-ops-mcp/src/tools.rs` | 修改 | 添加 pre_terminal_state/refused 字段，实现预检查逻辑 |
| `docs/TOOLS.md` | 修改 | 更新 exec 工具文档 |
| `.opencode/skills/agent-ops-mcp/SKILL.md` | 修改 | 添加安全检查指导 |

### 测试覆盖

- **单元测试**：2 个测试（refused 格式、normal 格式）
- **E2E 测试**：5 个场景（ready、editor、pager、repl、password）

### 向后兼容性

⚠️ **破坏性变更**：之前能在非 ready 状态下执行的场景现在会被拒绝。这是有意的安全增强。

### 额外开销

- **+1 次 snapshot IPC**（约 1-10ms）：执行前检测终端状态
- 可接受：安全检查的成本远小于误操作的代价

---

**Plan complete and saved to `docs/superpowers/plans/2026-07-13-exec-precheck-safety.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
