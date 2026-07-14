use anyhow::{Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid, recv_json_frame, send_json_frame};
use agent_ops_core::types::AuditAction;

/// 将字面量转义序列转为实际控制字符。
/// 兜底处理 OpenCode 等 MCP 客户端未正确 JSON-转义的情况。
pub(crate) fn unescape_keys(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('x') => {
                let hi = chars.next().unwrap_or('\0');
                let lo = chars.next().unwrap_or('\0');
                if let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) {
                    out.push(((h << 4) | l) as u8 as char);
                } else {
                    out.push_str(&format!("\\x{}{}", hi, lo));
                }
            }
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub(crate) async fn spawn_command(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "spawn_command", "session_name": session_name, "pane_id": pane_id, "command": command, "args": cmd_args })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SpawnCommand,
        host_name,
        session_name,
        Some(pane_id),
        command,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn shell_command(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "shell_command", "session_name": session_name, "pane_id": pane_id, "command": command })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::ShellCommand,
        host_name,
        session_name,
        Some(pane_id),
        command,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn broadcast_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_ids = args["pane_ids"].as_array().cloned().unwrap_or_default();
    let keys = args["keys"].as_str().context("missing 'keys'")?;
    let keys = unescape_keys(keys);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "broadcast_keys", "session_name": session_name, "pane_ids": pane_ids, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::BroadcastKeys,
        host_name,
        session_name,
        None,
        &format!("{} panes: {}", pane_ids.len(), keys),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn cmd_escape(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "cmd_escape", "host": host_name, "args": cmd_args }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    let stdout = response["stdout"]
        .as_str()
        .or_else(|| response["output"].as_str())
        .unwrap_or("");
    let stdout_sliced: String = stdout.chars().take(500).collect();
    let output_summary = if stdout_sliced.is_empty() {
        None
    } else {
        Some(stdout_sliced.as_str())
    };
    super::audit(
        ctx,
        AuditAction::CmdEscape,
        host_name,
        "",
        None,
        &format!("{:?}", cmd_args),
        output_summary,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

/// 单次命令执行的结果，用于 batch 聚合和单机 exec 复用
pub(crate) struct ExecResult {
    pub(crate) ok: bool,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: u64,
    pub(crate) error: Option<String>,
    pub(crate) terminal_state: Option<serde_json::Value>,
    pub(crate) cursor: Option<serde_json::Value>,
    pub(crate) pre_terminal_state: Option<serde_json::Value>,
    pub(crate) refused: bool,
}

/// 在已有 session + pane 中执行一次性命令并等待结果。
/// 抽取自 `exec` 函数，供 `exec` 和 `batch_exec` 复用。
/// 不负责建连、建 session、写 audit。
pub(crate) async fn exec_in_session<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    command: &str,
    timeout_ms: u64,
    max_lines: usize,
) -> ExecResult
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    // ── Pre-execution safety check ──
    // Capture terminal state before sending any keys. If the terminal is not in
    // "ready" state (e.g. editor, pager, password prompt), refuse to execute
    // to prevent command injection into non-shell contexts.
    let precheck_state = {
        let check_req = json!({
            "type": "capture_pane",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_lines": 1,
        });
        match send_json_frame(stream, &check_req).await {
            Ok(_) => match recv_json_frame(stream).await {
                Ok(resp) => resp.get("terminal_state").cloned(),
                Err(e) => {
                    tracing::warn!(error = %e, "precheck: failed to receive capture_pane response");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "precheck: failed to send capture_pane request");
                None
            }
        }
    };

    let is_ready = precheck_state
        .as_ref()
        .and_then(|v| v.as_str())
        .map(|s| s == "ready")
        .unwrap_or(true); // If detection fails, allow execution (backward compatible)

    if !is_ready {
        let state_name = precheck_state
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
            terminal_state: precheck_state.clone(),
            cursor: None,
            pre_terminal_state: precheck_state,
            refused: true,
        };
    }

    let marker_id = Uuid::new_v4().to_string();
    let marker_id = &marker_id[..3];
    let start_marker = format!("[{}]", marker_id);
    let sentinel_marker = format!("[{} ", marker_id);

    let keys = format!(
        "\x15echo '{s}'\n{c}\necho \"{e}$?]\"\n",
        s = start_marker,
        c = command,
        e = sentinel_marker
    );

    if let Err(e) = send_json_frame(
        stream,
        &json!({
            "type": "send_keys",
            "session_name": session_name,
            "pane_id": pane_id,
            "keys": keys,
        }),
    )
    .await
    {
        return ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(format!("send_keys: {e}")),
            terminal_state: None,
            cursor: None,
            pre_terminal_state: precheck_state.clone(),
            refused: false,
        };
    }

    let send_resp = match recv_json_frame(stream).await {
        Ok(r) => r,
        Err(e) => {
            return ExecResult {
                ok: false,
                output: String::new(),
                exit_code: None,
                duration_ms: 0,
                error: Some(format!("send_keys: {e}")),
                terminal_state: None,
                cursor: None,
                pre_terminal_state: precheck_state.clone(),
                refused: false,
            }
        }
    };

    if !send_resp["ok"].as_bool().unwrap_or(false) {
        let err = send_resp["error"]
            .as_str()
            .unwrap_or("send_keys failed")
            .to_string();
        return ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(err),
            terminal_state: None,
            cursor: None,
            pre_terminal_state: precheck_state.clone(),
            refused: false,
        };
    }

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let mut last_text = String::new();
    let mut last_terminal_state: Option<serde_json::Value> = None;
    let mut last_cursor: Option<serde_json::Value> = None;
    let mut poll_interval = std::time::Duration::from_millis(50);

    loop {
        if let Err(e) = send_json_frame(
            stream,
            &json!({
                "type": "capture_pane",
                "session_name": session_name,
                "pane_id": pane_id,
                "max_lines": max_lines,
            }),
        )
        .await
        {
            return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("capture_pane: {e}")),
                terminal_state: last_terminal_state,
                cursor: last_cursor,
                pre_terminal_state: precheck_state.clone(),
                refused: false,
            };
        }
        let resp = match recv_json_frame(stream).await {
            Ok(r) => r,
            Err(e) => {
                return ExecResult {
                    ok: false,
                    output: last_text,
                    exit_code: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("recv: {e}")),
                    terminal_state: last_terminal_state,
                    cursor: last_cursor,
                    pre_terminal_state: precheck_state.clone(),
                    refused: false,
                }
            }
        };
        last_text = resp["text"].as_str().unwrap_or("").to_string();
        last_terminal_state = resp.get("terminal_state").cloned();
        last_cursor = resp.get("cursor").cloned();

        if let Some(pos) = last_text.find(&sentinel_marker) {
            let after_sentinel = &last_text[pos + sentinel_marker.len()..];
            let exit_code: Option<i32> = after_sentinel
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok();

            let output_before_sentinel = &last_text[..pos];
            let current_output =
                if let Some(start_pos) = output_before_sentinel.rfind(&start_marker) {
                    let after_start = start_pos + start_marker.len();
                    &output_before_sentinel[after_start..]
                } else {
                    &last_text[..pos]
                };

            let output_lines: Vec<&str> = current_output
                .lines()
                .filter(|line| {
                    let t = line.trim();
                    if t == "clear" {
                        return false;
                    }
                    if t.starts_with(command) || t == command {
                        return false;
                    }
                    if t.starts_with("echo") && t.contains(&sentinel_marker) {
                        return false;
                    }
                    if t.starts_with('[')
                        && t.len() >= 4
                        && t.as_bytes().get(1).is_some_and(|b| b.is_ascii_hexdigit())
                        && t.as_bytes().get(2).is_some_and(|b| b.is_ascii_hexdigit())
                        && t.as_bytes().get(3).is_some_and(|b| b.is_ascii_hexdigit())
                        && t.get(4..).is_some_and(|s| s == "]" || s.starts_with(" "))
                    {
                        return false;
                    }
                    true
                })
                .collect();

            let output = output_lines.join("\n").trim().to_string();
            let duration_ms = start.elapsed().as_millis() as u64;

            return ExecResult {
                ok: exit_code == Some(0),
                output,
                exit_code,
                duration_ms,
                error: None,
                terminal_state: last_terminal_state,
                cursor: last_cursor,
                pre_terminal_state: precheck_state.clone(),
                refused: false,
            };
        }

        if std::time::Instant::now() >= deadline {
            let duration_ms = start.elapsed().as_millis() as u64;
            return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms,
                error: Some(format!(
                    "timeout waiting for sentinel after {}ms",
                    timeout_ms
                )),
                terminal_state: last_terminal_state,
                cursor: last_cursor,
                pre_terminal_state: precheck_state.clone(),
                refused: false,
            };
        }

        tokio::time::sleep(poll_interval).await;
        poll_interval = std::cmp::min(poll_interval * 2, std::time::Duration::from_millis(500));
    }
}

pub(crate) async fn exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let max_lines = args["max_lines"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(200);
    let clear_screen = args["clear_screen"].as_bool().unwrap_or(false);

    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    // clear_screen 在 exec_in_session 之前单独处理
    if clear_screen {
        let _ = send_json_frame(
            &mut tls,
            &json!({
                "type": "send_keys", "session_name": session_name, "pane_id": pane_id,
                "keys": "clear\n",
            }),
        )
        .await;
        let _ = recv_json_frame(&mut tls).await;
    }

    let result = exec_in_session(
        &mut tls,
        session_name,
        pane_id,
        command,
        timeout_ms,
        max_lines,
    )
    .await;

    let output_summary: String = result.output.chars().take(500).collect();
    super::audit(
        ctx,
        AuditAction::Exec,
        host_name,
        session_name,
        Some(pane_id),
        command,
        Some(&output_summary),
        result.ok,
        result.duration_ms,
        result.error.as_deref(),
    )
    .await;

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
    if let Some(ref pre_state) = result.pre_terminal_state {
        response["pre_terminal_state"] = pre_state.clone();
    }
    if result.refused {
        response["refused"] = json!(true);
    }
    Ok(response)
}

pub(crate) async fn collect_until_exit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(1048576);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(60000);
    let starting_at = args["starting_at"].as_str().unwrap_or("now");
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(
        &mut tls,
        &json!({
            "type": "collect_until_exit",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_bytes": max_bytes,
            "timeout_ms": timeout_ms,
            "starting_at": starting_at,
        }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::CollectUntilExit,
        host_name,
        session_name,
        Some(pane_id),
        "",
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}
