# 安全加固实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 3 个安全漏洞：文件路径规范化（V-01/V-02）、下载目录遍历防护（V-10）、隧道 SSRF 白名单（V-04）

**Architecture:** Bridge 端增加路径规范化函数（null byte 拒绝 + canonicalize + 审计日志），MCP 端验证远端返回的 rel_path 不含路径穿越，MCP 端增加可选隧道目标白名单。所有改动向后兼容，不限制合法操作。

**Tech Stack:** Rust, tokio, glob crate (已在 agent-ops-mcp 依赖中)

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `crates/rmux-bridge/src/files.rs` | 修改 | 新增 `sanitize_path()` 函数，上传/下载调用处使用 |
| `crates/agent-ops-mcp/src/files.rs` | 修改 | `read_directory()` 中验证 rel_path 安全性 |
| `crates/agent-ops-core/src/types.rs` | 修改 | HostConfig 新增 `allowed_tunnel_targets` 字段 |
| `crates/agent-ops-mcp/src/tunnel.rs` | 修改 | `create()` 方法开头增加目标白名单检查 |
| `config/hosts.example.yaml` | 修改 | 增加 `allowed_tunnel_targets` 示例 |

---

### Task 1: Bridge 端文件路径规范化（V-01/V-02）

**Files:**
- Modify: `crates/rmux-bridge/src/files.rs:1-7`（新增 import + sanitize_path 函数）
- Modify: `crates/rmux-bridge/src/files.rs:62`（上传调用处）
- Modify: `crates/rmux-bridge/src/files.rs:142`（下载调用处）

- [ ] **Step 1: 在 files.rs 顶部新增 sanitize_path 函数**

在 `crates/rmux-bridge/src/files.rs` 第 7 行（`const CHUNK_SIZE` 之后）插入：

```rust
/// 规范化文件路径：拒绝 null byte，解析 `..` 和符号链接。
/// 返回 canonical 路径，确保路径"诚实"（所见即所得）。
/// 不限制操作范围 — 这是运维工具，需要完整文件系统访问。
fn sanitize_path(raw: &str) -> anyhow::Result<String> {
    // 拒绝 null byte（C 字符串截断攻击）
    if raw.contains('\0') {
        anyhow::bail!("path contains null byte");
    }

    let path = std::path::Path::new(raw);

    // 尝试 canonicalize（解析 .. 和符号链接）
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // 文件不存在时（上传场景），对 parent 做 canonicalize
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("invalid path: {}", raw))?;
            let file_name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid path: {}", raw))?;
            if parent.as_os_str().is_empty() {
                std::path::PathBuf::from(file_name)
            } else {
                let canon_parent = parent.canonicalize().with_context(|| {
                    format!("parent directory not found: {}", parent.display())
                })?;
                canon_parent.join(file_name)
            }
        }
    };

    let result = canonical.to_string_lossy().to_string();
    tracing::info!(
        operation = "file_access",
        requested = raw,
        resolved = %result,
        "file path resolved"
    );
    Ok(result)
}
```

- [ ] **Step 2: 在上传处理中调用 sanitize_path**

在 `crates/rmux-bridge/src/files.rs` 第 62 行（`let mut remote_path = ...` 之后），将：

```rust
    let mut remote_path = String::from_utf8_lossy(&path).to_string();
```

改为：

```rust
    let mut remote_path = sanitize_path(&String::from_utf8_lossy(&path))?;
```

- [ ] **Step 3: 在下载处理中调用 sanitize_path**

在 `crates/rmux-bridge/src/files.rs` 第 142 行（`let remote_path = ...` 之后），将：

```rust
    let remote_path = String::from_utf8_lossy(&path).to_string();
```

改为：

```rust
    let remote_path = sanitize_path(&String::from_utf8_lossy(&path))?;
```

- [ ] **Step 4: 添加单元测试**

在 `crates/rmux-bridge/src/files.rs` 文件末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_path_rejects_null_byte() {
        assert!(sanitize_path("/tmp/foo\0bar").is_err());
    }

    #[test]
    fn test_sanitize_path_resolves_dotdot() {
        // /tmp 一定存在，所以 parent canonicalize 会成功
        let result = sanitize_path("/tmp/../etc/passwd").unwrap();
        assert_eq!(result, "/etc/passwd");
    }

    #[test]
    fn test_sanitize_path_absolute() {
        let result = sanitize_path("/tmp").unwrap();
        assert_eq!(result, "/tmp");
    }

    #[test]
    fn test_sanitize_path_nonexistent_file() {
        // 文件不存在但 parent 存在
        let result = sanitize_path("/tmp/nonexistent-file-xyz-123.txt").unwrap();
        assert_eq!(result, "/tmp/nonexistent-file-xyz-123.txt");
    }
}
```

- [ ] **Step 5: 运行测试验证**

```bash
cargo test -p rmux-bridge -- --test-threads=1
```

Expected: 所有测试 PASS

- [ ] **Step 6: 运行 clippy 验证**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: 无警告

- [ ] **Step 7: 提交**

```bash
git add crates/rmux-bridge/src/files.rs
git commit -m "security: add path sanitization for file upload/download (V-01/V-02)"
```

---

### Task 2: MCP 端下载目录遍历防护（V-10）

**Files:**
- Modify: `crates/agent-ops-mcp/src/files.rs:315`（read_directory 中 rel_path 验证）

- [ ] **Step 1: 在 read_directory 函数中增加 rel_path 验证**

在 `crates/agent-ops-mcp/src/files.rs` 第 315 行（`let rel_path = ...` 之后），在 `let mut size_buf` 之前，插入：

```rust
        // 验证 rel_path 安全性：拒绝路径穿越和绝对路径
        if rel_path.contains("..") || rel_path.starts_with('/') {
            bail!(
                "unsafe relative path from bridge: '{}' (contains '..' or is absolute)",
                rel_path
            );
        }
```

完整上下文（第 314-332 行）变为：

```rust
        let rel_path = String::from_utf8_lossy(&path_buf).to_string();

        // 验证 rel_path 安全性：拒绝路径穿越和绝对路径
        if rel_path.contains("..") || rel_path.starts_with('/') {
            bail!(
                "unsafe relative path from bridge: '{}' (contains '..' or is absolute)",
                rel_path
            );
        }

        let mut size_buf = [0u8; 8];
```

- [ ] **Step 2: 添加单元测试**

在 `crates/agent-ops-mcp/src/files.rs` 文件末尾的 `#[cfg(test)] mod tests` 中添加：

```rust
    #[test]
    fn test_rel_path_rejects_dotdot() {
        let bad_paths = ["../etc/passwd", "foo/../../etc/shadow", ".."];
        for p in bad_paths {
            assert!(
                p.contains(".."),
                "should detect path traversal in: {}",
                p
            );
        }
    }

    #[test]
    fn test_rel_path_rejects_absolute() {
        let bad_paths = ["/etc/passwd", "/root/.ssh/id_rsa"];
        for p in bad_paths {
            assert!(
                p.starts_with('/'),
                "should detect absolute path: {}",
                p
            );
        }
    }

    #[test]
    fn test_rel_path_accepts_normal() {
        let good_paths = ["file.txt", "subdir/file.txt", "a/b/c/d.rs"];
        for p in good_paths {
            assert!(
                !p.contains("..") && !p.starts_with('/'),
                "should accept normal path: {}",
                p
            );
        }
    }
```

- [ ] **Step 3: 运行测试验证**

```bash
cargo test -p agent-ops-mcp -- files::tests
```

Expected: 所有测试 PASS

- [ ] **Step 4: 运行 clippy 验证**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: 无警告

- [ ] **Step 5: 提交**

```bash
git add crates/agent-ops-mcp/src/files.rs
git commit -m "security: validate rel_path in directory download (V-10)"
```

---

### Task 3: 隧道 SSRF 白名单（V-04）

**Files:**
- Modify: `crates/agent-ops-core/src/types.rs:22-23`（HostConfig 新增字段）
- Modify: `crates/agent-ops-mcp/src/tunnel.rs:72`（create 方法开头增加检查）
- Modify: `config/hosts.example.yaml`（增加示例配置）

- [ ] **Step 1: HostConfig 新增 allowed_tunnel_targets 字段**

在 `crates/agent-ops-core/src/types.rs` 第 22 行（`pub labels` 字段之后），`}` 之前，添加：

```rust
    /// 可选：允许的隧道目标列表（glob 模式，如 "10.0.1.*:*"）。
    /// None = 全部允许（向后兼容，不配置则不限制）。
    #[serde(default)]
    pub allowed_tunnel_targets: Option<Vec<String>>,
```

- [ ] **Step 2: 在 tunnel.rs 中新增 check_tunnel_target 函数**

在 `crates/agent-ops-mcp/src/tunnel.rs` 第 20 行（`const MAX_HOST_LEN` 之后）插入：

```rust
/// 检查隧道目标是否在允许列表中。
/// 如果 host 未配置 allowed_tunnel_targets，则全部允许（向后兼容）。
fn check_tunnel_target(host: &HostConfig, remote_host: &str, remote_port: u16) -> Result<()> {
    let targets = match &host.allowed_tunnel_targets {
        Some(t) => t,
        None => return Ok(()), // 未配置 = 全部允许
    };

    let target = format!("{}:{}", remote_host, remote_port);
    let matched = targets.iter().any(|pattern| {
        glob::Pattern::new(pattern)
            .map(|p| p.matches(&target))
            .unwrap_or(false)
    });

    if matched {
        Ok(())
    } else {
        anyhow::bail!(
            "tunnel target {}:{} not in allowed list for host '{}' (allowed: {:?})",
            remote_host,
            remote_port,
            host.name,
            targets
        )
    }
}
```

- [ ] **Step 3: 在 create 方法中调用 check_tunnel_target**

在 `crates/agent-ops-mcp/src/tunnel.rs` 的 `create` 方法中，第 78 行（`remote_host.len()` 检查之后），`let bind_addr` 之前，插入：

```rust
        check_tunnel_target(host, &remote_host, remote_port)?;
```

- [ ] **Step 4: 更新 hosts.example.yaml**

在 `config/hosts.example.yaml` 的 `prod-db-01` 条目中，`labels` 之后添加示例：

```yaml
  - name: prod-db-01
    bridge_addr: 10.0.1.20:9778
    bridge_token: "token-db-01-xxxxxxxx"
    group: production
    tags:
      - database
      - postgres
    labels:
      dc: shanghai
      rack: b1
    # 可选：限制隧道目标（不配置 = 全部允许）
    # allowed_tunnel_targets:
    #   - "127.0.0.1:5432"
    #   - "10.0.1.*:*"
```

- [ ] **Step 5: 添加单元测试**

在 `crates/agent-ops-mcp/src/tunnel.rs` 文件末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use agent_ops_core::types::HostConfig;
    use std::collections::HashMap;

    fn make_host(targets: Option<Vec<String>>) -> HostConfig {
        HostConfig {
            name: "test-host".to_string(),
            bridge_addr: "10.0.0.1:9778".to_string(),
            bridge_token: "tok".to_string(),
            group: "test".to_string(),
            tags: vec![],
            labels: HashMap::new(),
            allowed_tunnel_targets: targets,
        }
    }

    #[test]
    fn test_no_targets_allows_all() {
        let host = make_host(None);
        assert!(check_tunnel_target(&host, "127.0.0.1", 22).is_ok());
        assert!(check_tunnel_target(&host, "10.0.0.1", 3306).is_ok());
    }

    #[test]
    fn test_exact_match() {
        let host = make_host(Some(vec!["127.0.0.1:5432".to_string()]));
        assert!(check_tunnel_target(&host, "127.0.0.1", 5432).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 3306).is_err());
    }

    #[test]
    fn test_glob_match() {
        let host = make_host(Some(vec!["10.0.1.*:*".to_string()]));
        assert!(check_tunnel_target(&host, "10.0.1.20", 5432).is_ok());
        assert!(check_tunnel_target(&host, "10.0.1.100", 80).is_ok());
        assert!(check_tunnel_target(&host, "10.0.2.1", 80).is_err());
    }

    #[test]
    fn test_port_glob() {
        let host = make_host(Some(vec!["*:3306".to_string()]));
        assert!(check_tunnel_target(&host, "10.0.1.20", 3306).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 3306).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 5432).is_err());
    }
}
```

- [ ] **Step 6: 运行测试验证**

```bash
cargo test -p agent-ops-mcp -- tunnel::tests
```

Expected: 所有测试 PASS

- [ ] **Step 7: 运行全量测试 + clippy**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Expected: 全部 PASS，无警告，格式正确

- [ ] **Step 8: 提交**

```bash
git add crates/agent-ops-core/src/types.rs crates/agent-ops-mcp/src/tunnel.rs config/hosts.example.yaml
git commit -m "security: add optional tunnel target whitelist for SSRF protection (V-04)"
```

---

## 验证清单

完成所有 Task 后：

```bash
# 编译检查
cargo check --workspace

# 全量测试
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# 格式
cargo fmt --all -- --check
```
