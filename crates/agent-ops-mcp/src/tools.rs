use agent_ops_core::types::{AuditAction, AuditEvent};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::audit;
use crate::router::HostRouter;
use crate::files::OverwriteMode;
use crate::stream::StreamManager;
use crate::transport::{connect_to_bridge_hybrid, send_json_frame, recv_json_frame};
use crate::transport::BridgeStream;
use crate::tunnel::TunnelManager;

pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub insecure: bool,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: std::sync::Mutex<String>,
    pub tunnel_manager: Arc<TunnelManager>,
    pub stream_manager: Arc<StreamManager>,
}

pub async fn execute_tool(ctx: &ToolContext, tool_name: &str, args: Value) -> Result<Value> {
    match tool_name {
        "agent_ops_usage_rules" => Ok(json!({})),
        "host_list" => host_list(ctx).await,
        "host_filter" => host_filter(ctx, args).await,
        "session_create" => session_create(ctx, args).await,
        "session_list" => session_list(ctx, args).await,
        "session_attach" => session_attach(ctx, args).await,
        "session_detach" => session_detach(ctx, args).await,
        "send_keys" => send_keys(ctx, args).await,
        "capture_pane" => capture_pane(ctx, args).await,
        "wait_for_text" => wait_for_text(ctx, args).await,
        "wait_exit" => wait_exit(ctx, args).await,
        "spawn_command" => spawn_command(ctx, args).await,
        "shell_command" => shell_command(ctx, args).await,
        "respawn_pane" => respawn_pane(ctx, args).await,
        "broadcast_keys" => broadcast_keys(ctx, args).await,
        "cmd_escape" => cmd_escape(ctx, args).await,
        "split_window" => split_window(ctx, args).await,
        "stream_pane" => stream_pane(ctx, args).await,
        "file_upload" => file_upload(ctx, args).await,
        "file_download" => file_download(ctx, args).await,
        "exec" => exec(ctx, args).await,
        "close_pane" => close_pane(ctx, args).await,
        "split_pane" => split_pane(ctx, args).await,
        "resize_pane" => resize_pane(ctx, args).await,
        "send_text" => send_text(ctx, args).await,
        "set_pane_title" => set_pane_title(ctx, args).await,
        "find_pane_text" => find_pane_text(ctx, args).await,
        "close_window" => close_window(ctx, args).await,
        "kill_session" => kill_session(ctx, args).await,
        "rename_window" => rename_window(ctx, args).await,
        "list_window_panes" => list_window_panes(ctx, args).await,
        "resize_window" => resize_window(ctx, args).await,
        "select_window" => select_window(ctx, args).await,
        "select_layout" => select_layout(ctx, args).await,
        "pane_info" => pane_info(ctx, args).await,
        "window_info" => window_info(ctx, args).await,
        "pane_exists" => pane_exists(ctx, args).await,
        "batch_exec" => batch_exec(ctx, args).await,
        "batch_upload" => batch_upload(ctx, args).await,
        "batch_download" => batch_download(ctx, args).await,
        "tunnel_create" => tunnel_create(ctx, args).await,
        "tunnel_list" => tunnel_list(ctx).await,
        "tunnel_close" => tunnel_close(ctx, args).await,
        "find_panes" => find_panes(ctx, args).await,
        "find_sessions" => find_sessions(ctx, args).await,
        "get_pane_title" => get_pane_title(ctx, args).await,
        "find_text_all" => find_text_all(ctx, args).await,
        "clear_history" => clear_history(ctx, args).await,
        "list_buffers" => list_buffers(ctx, args).await,
        "paste_buffer" => paste_buffer(ctx, args).await,
        "delete_buffer" => delete_buffer(ctx, args).await,
        "split_pane_with" => split_pane_with(ctx, args).await,
        "get_pane_by_title" => get_pane_by_title(ctx, args).await,
        "collect_until_exit" => collect_until_exit(ctx, args).await,
        "break_pane" => break_pane(ctx, args).await,
        "join_pane" => join_pane(ctx, args).await,
        "swap_pane" => swap_pane(ctx, args).await,
        "host_capabilities" => host_capabilities(ctx, args).await,
        "capture_region" => capture_region(ctx, args).await,
        "wait_for_bytes" => wait_for_bytes(ctx, args).await,
        "wait_stable" => wait_stable(ctx, args).await,
        "deploy_bridge" => deploy_bridge(ctx, args).await,
        _ => anyhow::bail!("unknown tool: {}", tool_name),
    }
}

async fn host_list(ctx: &ToolContext) -> Result<Value> {
    let hosts: Vec<Value> = ctx
        .router
        .list()
        .iter()
        .map(|h| json!({
            "name": h.name,
            "group": h.group,
            "tags": h.tags,
            "labels": h.labels,
            "bridge_addr": h.bridge_addr,
        }))
        .collect();
    audit(ctx, AuditAction::HostList, "", "", None, "", None, true, 0, None).await;
    Ok(json!({ "hosts": hosts, "count": hosts.len() }))
}

async fn host_filter(ctx: &ToolContext, args: Value) -> Result<Value> {
    let mut hosts: Vec<&agent_ops_core::types::HostConfig> = ctx.router.list();

    if let Some(group) = args["group"].as_str() {
        hosts.retain(|h| h.group == group);
    }
    if let Some(tags) = args["tags"].as_array() {
        let tags: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        hosts.retain(|h| tags.iter().all(|t| h.tags.contains(&t.to_string())));
    }
    if let Some(key) = args["label_key"].as_str() {
        if let Some(value) = args["label_value"].as_str() {
            hosts.retain(|h| h.labels.get(key).map(|v| v == value).unwrap_or(false));
        }
    }
    if let Some(pattern) = args["pattern"].as_str() {
        if let Ok(pat) = glob::Pattern::new(pattern) {
            hosts.retain(|h| pat.matches(&h.name));
        }
    }

    let result: Vec<Value> = hosts
        .iter()
        .map(|h| json!({ "name": h.name, "group": h.group, "tags": h.tags, "labels": h.labels, "bridge_addr": h.bridge_addr }))
        .collect();
    audit(ctx, AuditAction::HostFilter, "", "", None, "",
        Some(&format!("group={:?} tags={:?} pattern={:?} label_key={:?}",
            args.get("group"), args.get("tags"), args.get("pattern"), args.get("label_key"))),
        true, 0, None).await;
    Ok(json!({ "hosts": result, "count": result.len() }))
}

async fn session_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("agent-ops");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    let request = json!({ "type": "new_session", "name": session_name, "detached": true });
    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;

    audit(ctx, AuditAction::SessionCreate, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn session_list(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "list_sessions" })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionList, host_name, "", None, "", None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn session_attach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "attach_session", "session_name": session_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionAttach, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn session_detach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "detach_session", "session_name": session_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionDetach, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

/// 将字面量转义序列转为实际控制字符。
/// 兜底处理 OpenCode 等 MCP 客户端未正确 JSON-转义的情况。
fn unescape_keys(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n')  => out.push('\n'),
            Some('r')  => out.push('\r'),
            Some('t')  => out.push('\t'),
            Some('e')  => out.push('\x1b'),
            Some('x')  => {
                let hi = chars.next().unwrap_or('\0');
                let lo = chars.next().unwrap_or('\0');
                if let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) {
                    out.push(((h << 4) | l) as u8 as char);
                } else {
                    out.push_str(&format!("\\x{}{}", hi, lo));
                }
            }
            Some('\\') => out.push('\\'),
            Some(other) => { out.push('\\'); out.push(other); }
            None => out.push('\\'),
        }
    }
    out
}

async fn send_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let raw_keys = args["keys"].as_str().context("missing 'keys'")?;
    let keys = unescape_keys(raw_keys);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "send_keys", "session_name": session_name, "pane_id": pane_id, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SendKeys, host_name, session_name, Some(pane_id), &keys, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn capture_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    let mut request = json!({
        "type": "capture_pane",
        "session_name": session_name,
        "pane_id": pane_id,
        "max_lines": max_lines,
    });
    if let Some(v) = args.get("ansi") { request["ansi"] = v.clone(); }
    if let Some(v) = args.get("start_line") { request["start_line"] = v.clone(); }
    if let Some(v) = args.get("end_line") { request["end_line"] = v.clone(); }
    if let Some(v) = args.get("join_wrapped") { request["join_wrapped"] = v.clone(); }
    if let Some(v) = args.get("preserve_spaces") { request["preserve_spaces"] = v.clone(); }
    if let Some(v) = args.get("alternate") { request["alternate"] = v.clone(); }
    if let Some(v) = args.get("buffer_name") { request["buffer_name"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    let num_lines = response["text"].as_str().map(|s| s.lines().count()).unwrap_or(0);
    audit(ctx, AuditAction::CapturePane, host_name, session_name, Some(pane_id), &format!("{} lines", num_lines), None, true, 0, None).await;
    Ok(response)
}

async fn spawn_command(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "spawn_command", "session_name": session_name, "pane_id": pane_id, "command": command, "args": cmd_args })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SpawnCommand, host_name, session_name, Some(pane_id), command, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn shell_command(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "shell_command", "session_name": session_name, "pane_id": pane_id, "command": command })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ShellCommand, host_name, session_name, Some(pane_id), command, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn broadcast_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_ids = args["pane_ids"].as_array().cloned().unwrap_or_default();
    let keys = args["keys"].as_str().context("missing 'keys'")?;
    let keys = unescape_keys(keys);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "broadcast_keys", "session_name": session_name, "pane_ids": pane_ids, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::BroadcastKeys, host_name, session_name, None, &format!("{} panes: {}", pane_ids.len(), keys), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn cmd_escape(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "cmd_escape", "host": host_name, "args": cmd_args })).await?;
    let response = recv_json_frame(&mut tls).await?;
    let stdout = response["stdout"].as_str().or_else(|| response["output"].as_str()).unwrap_or("");
    let stdout_sliced: String = stdout.chars().take(500).collect();
    let output_summary = if stdout_sliced.is_empty() { None } else { Some(stdout_sliced.as_str()) };
    audit(ctx, AuditAction::CmdEscape, host_name, "", None, &format!("{:?}", cmd_args), output_summary, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn respawn_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    let mut request = json!({
        "type": "respawn_pane",
        "session_name": session_name,
        "pane_id": pane_id,
    });
    if let Some(v) = args.get("command") { request["command"] = v.clone(); }
    if let Some(v) = args.get("args") { request["args"] = v.clone(); }
    if let Some(v) = args.get("shell") { request["shell"] = v.clone(); }
    if let Some(v) = args.get("cwd") { request["cwd"] = v.clone(); }
    if let Some(v) = args.get("env") { request["env"] = v.clone(); }
    if let Some(v) = args.get("kill") { request["kill"] = v.clone(); }
    if let Some(v) = args.get("keep_alive_on_exit") { request["keep_alive_on_exit"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::RespawnPane, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn wait_exit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "wait_exit", "session_name": session_name, "pane_id": pane_id, "timeout_ms": timeout_ms })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::WaitExit, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn wait_for_text(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let text = args["text"].as_str().context("missing 'text'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "wait_for_text", "session_name": session_name, "pane_id": pane_id, "text": text, "timeout_ms": timeout_ms })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::WaitForText, host_name, session_name, Some(pane_id), text, None, response["found"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn split_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let direction = args["direction"].as_str().unwrap_or("horizontal");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({ "type": "split_window", "session_name": session_name, "direction": direction })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SplitWindow, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let start = std::time::Instant::now();
    let response = ctx.stream_manager.stream_pane(host, session_name, pane_id, timeout_ms, ctx.ca_cert_path.as_deref(), ctx.insecure).await?;
    let elapsed = start.elapsed().as_millis() as u64;

    let has_data = response["text"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
    audit(ctx, AuditAction::StreamSubscribe, host_name, session_name, Some(pane_id),
        "", None, has_data, elapsed, None).await;

    Ok(response)
}

async fn file_upload(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let local_path = args["local_path"].as_str().context("missing 'local_path'")?;
    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let overwrite = match args["overwrite"].as_str().unwrap_or("overwrite") {
        "skip" => crate::files::OverwriteMode::Skip,
        "rename" => crate::files::OverwriteMode::Rename,
        "error" => crate::files::OverwriteMode::NoClobber,
        _ => crate::files::OverwriteMode::Overwrite,
    };
    let exclude: Vec<String> = args["exclude"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let result = crate::files::upload_file(
        host,
        local_path,
        remote_path,
        ctx.ca_cert_path.as_deref(),
        ctx.insecure,
        overwrite,
        &exclude,
    )
    .await;
    audit(ctx, AuditAction::FileUpload, host_name, "", None, local_path, None, result.is_ok(), 0, None).await;

    match result {
        Ok(files) => Ok(json!({
            "ok": true,
            "files": files,
            "total": files.len(),
            "uploaded": files.iter().filter(|f| f.status == "uploaded").count(),
            "skipped": files.iter().filter(|f| f.status == "skipped").count(),
            "failed": 0,
        })),
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn file_download(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let local_path = args["local_path"].as_str().context("missing 'local_path'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let result = crate::files::download_file(
        host,
        remote_path,
        local_path,
        ctx.ca_cert_path.as_deref(),
        ctx.insecure,
    )
    .await;
    audit(ctx, AuditAction::FileDownload, host_name, "", None, remote_path, None, result.is_ok(), 0, None).await;

    match result {
        Ok(files) => {
            if files.len() == 1 {
                Ok(json!({
                    "ok": true,
                    "file": {
                        "uri": format!("file://{}/{}", host_name, remote_path),
                        "local_path": files[0].path,
                        "size": files[0].size,
                        "sha256": files[0].sha256,
                    }
                }))
            } else {
                Ok(json!({
                    "ok": true,
                    "files": files,
                    "total": files.len(),
                }))
            }
        }
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// 单次命令执行的结果，用于 batch 聚合和单机 exec 复用
struct ExecResult {
    ok: bool,
    output: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    error: Option<String>,
}

/// 在已有 session + pane 中执行一次性命令并等待结果。
/// 抽取自 `exec` 函数，供 `exec` 和 `batch_exec` 复用。
/// 不负责建连、建 session、写 audit。
async fn exec_in_session<S>(
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
    let marker_id = Uuid::new_v4().to_string();
    let marker_id = &marker_id[..3];
    let start_marker = format!("[{}]", marker_id);
    let sentinel_marker = format!("[{} ", marker_id);

    let keys = format!("\x15echo '{s}'\n{c}\necho \"{e}$?]\"\n",
        s = start_marker, c = command, e = sentinel_marker);

    if let Err(e) = send_json_frame(stream, &json!({
        "type": "send_keys",
        "session_name": session_name,
        "pane_id": pane_id,
        "keys": keys,
    })).await {
        return ExecResult {
            ok: false, output: String::new(), exit_code: None,
            duration_ms: 0, error: Some(format!("send_keys: {e}")),
        };
    }

    let send_resp = match recv_json_frame(stream).await {
        Ok(r) => r,
        Err(e) => return ExecResult {
            ok: false, output: String::new(), exit_code: None,
            duration_ms: 0, error: Some(format!("send_keys: {e}")),
        },
    };

    if !send_resp["ok"].as_bool().unwrap_or(false) {
        let err = send_resp["error"].as_str().unwrap_or("send_keys failed").to_string();
        return ExecResult {
            ok: false, output: String::new(), exit_code: None,
            duration_ms: 0, error: Some(err),
        };
    }

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let mut last_text = String::new();
    let mut poll_interval = std::time::Duration::from_millis(50);

    loop {
        if let Err(e) = send_json_frame(stream, &json!({
            "type": "capture_pane",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_lines": max_lines,
        })).await {
            return ExecResult {
                ok: false, output: last_text, exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("capture_pane: {e}")),
            };
        }
        let resp = match recv_json_frame(stream).await {
            Ok(r) => r,
            Err(e) => return ExecResult {
                ok: false, output: last_text, exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("recv: {e}")),
            },
        };
        last_text = resp["text"].as_str().unwrap_or("").to_string();

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
            let current_output = if let Some(start_pos) = output_before_sentinel.rfind(&start_marker) {
                let after_start = start_pos + start_marker.len();
                &output_before_sentinel[after_start..]
            } else {
                &last_text[..pos]
            };

            let output_lines: Vec<&str> = current_output
                .lines()
                .filter(|line| {
                    let t = line.trim();
                    if t == "clear" { return false; }
                    if t.starts_with(command) || t == command { return false; }
                    if t.starts_with("echo") && t.contains(&sentinel_marker) { return false; }
                    if t.starts_with('[') && t.len() >= 4
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
            };
        }

        if std::time::Instant::now() >= deadline {
            let duration_ms = start.elapsed().as_millis() as u64;
            return ExecResult {
                ok: false,
                output: last_text,
                exit_code: None,
                duration_ms,
                error: Some(format!("timeout waiting for sentinel after {}ms", timeout_ms)),
            };
        }

        tokio::time::sleep(poll_interval).await;
        poll_interval = std::cmp::min(poll_interval * 2, std::time::Duration::from_millis(500));
    }
}

async fn exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let clear_screen = args["clear_screen"].as_bool().unwrap_or(false);

    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    // clear_screen 在 exec_in_session 之前单独处理
    if clear_screen {
        let _ = send_json_frame(&mut tls, &json!({
            "type": "send_keys", "session_name": session_name, "pane_id": pane_id,
            "keys": "clear\n",
        })).await;
        let _ = recv_json_frame(&mut tls).await;
    }

    let result = exec_in_session(&mut tls, session_name, pane_id, command, timeout_ms, max_lines).await;

    let output_summary: String = result.output.chars().take(500).collect();
    audit(ctx, AuditAction::Exec, host_name, session_name, Some(pane_id), command,
        Some(&output_summary), result.ok, result.duration_ms, result.error.as_deref()).await;

    Ok(json!({
        "ok": result.ok,
        "output": result.output,
        "exit_code": result.exit_code,
        "duration_ms": result.duration_ms,
        "error": result.error,
    }))
}

/// 内部 session_create（不记 audit）
async fn create_session_inner(stream: &mut BridgeStream, session_name: &str) -> Result<Value> {
    send_json_frame(stream, &json!({ "type": "new_session", "name": session_name, "detached": true })).await?;
    recv_json_frame(stream).await
}

/// 解析主机名列表 → (name, Option<HostConfig>)
fn resolve_hosts(ctx: &ToolContext, names: &[String]) -> Vec<(String, Option<agent_ops_core::types::HostConfig>)> {
    names.iter().map(|name| {
        let h = ctx.router.get(name);
        (name.clone(), h.cloned())
    }).collect()
}

/// 创建并发信号量（concurrency=0 → None，即不限制）
fn make_semaphore(limit: usize) -> Option<Arc<tokio::sync::Semaphore>> {
    if limit > 0 { Some(Arc::new(tokio::sync::Semaphore::new(limit))) } else { None }
}

/// 收集 JoinHandle 结果 → (results_map, success_count, failed_count)
async fn collect_batch_results(
    handles: Vec<tokio::task::JoinHandle<(String, Value)>>,
) -> (serde_json::Map<String, Value>, u32, u32) {
    let mut results_map = serde_json::Map::new();
    let mut success = 0u32;
    let mut failed = 0u32;
    for handle in handles {
        if let Ok((host_name, result)) = handle.await {
            if result["ok"].as_bool().unwrap_or(false) { success += 1; } else { failed += 1; }
            results_map.insert(host_name, result);
        } else {
            failed += 1;
            results_map.insert("unknown".into(), json!({"ok": false, "error": "task cancelled"}));
        }
    }
    (results_map, success, failed)
}

async fn batch_exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({ "ok": true, "command": "", "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list" }));
    }

    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(120000);
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let cmd = command.to_string();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let cmd = cmd.clone();
        let sem = semaphore.clone();

        let handle = tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else { None };

            let host = match host_opt {
                Some(h) => h,
                None => return (host_name, json!({
                    "ok": false, "output": "", "exit_code": null,
                    "duration_ms": 0, "error": "host not found in registry",
                })),
            };

            let mut stream = match connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                ca_cert.as_deref(), 3, insecure,
            ).await {
                Ok(s) => s,
                Err(e) => return (host_name, json!({
                    "ok": false, "output": "", "exit_code": null,
                    "duration_ms": 0, "error": format!("connect: {e}"),
                })),
            };

            let session_name = "agent-ops";

            // 创建 session 并获取 pane_id
            let pane_id = match create_session_inner(&mut stream, session_name).await {
                Ok(resp) => resp.get("pane_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("%0")
                    .to_string(),
                Err(e) => return (host_name, json!({
                    "ok": false, "output": "", "exit_code": null,
                    "duration_ms": 0, "error": format!("session_create: {e}"),
                })),
            };

            let result = exec_in_session(&mut stream, session_name, &pane_id, &cmd, timeout_ms, max_lines).await;

            (host_name, json!({
                "ok": result.ok && result.error.is_none(),
                "output": result.output,
                "exit_code": result.exit_code,
                "duration_ms": result.duration_ms,
                "error": result.error,
            }))
        });

        handles.push(handle);
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;

    audit(ctx, AuditAction::BatchExec, "", "", None,
        &format!("hosts:{:?} cmd:{}", hosts_arg, cmd), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0,
        "command": command,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}

async fn batch_upload(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array().context("missing 'hosts'")?
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
    }

    let local_path = args["local_path"].as_str().context("missing 'local_path'")?;
    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let overwrite = match args["overwrite"].as_str().unwrap_or("overwrite") {
        "skip" => OverwriteMode::Skip,
        "rename" => OverwriteMode::Rename,
        "error" => OverwriteMode::NoClobber,
        _ => OverwriteMode::Overwrite,
    };
    let exclude: Vec<String> = args["exclude"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let local = local_path.to_string();
        let remote = remote_path.to_string();
        let exclude = exclude.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };
            let host = match host_opt {
                Some(h) => h,
                None => return (host_name, json!({"ok": false, "error": "host not found"})),
            };
            match crate::files::upload_file(&host, &local, &remote, ca_cert.as_deref(), insecure, overwrite, &exclude).await {
                Ok(files) => {
                    let uploaded = files.iter().filter(|f| f.status == "uploaded").count();
                    let file_failed = files.iter().filter(|f| f.status == "failed").count();
                    (host_name, json!({
                        "ok": file_failed == 0,
                        "files": files, "total": files.len(),
                        "uploaded": uploaded, "skipped": files.len() - uploaded - file_failed,
                        "failed_count": file_failed,
                    }))
                }
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    audit(ctx, AuditAction::BatchUpload, "", "", None,
        &format!("hosts:{:?} local:{}", hosts_arg, local_path), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}

async fn batch_download(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array().context("missing 'hosts'")?
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
    }

    let remote_path = args["remote_path"].as_str().context("missing 'remote_path'")?;
    let local_dir = args["local_dir"].as_str().context("missing 'local_dir'")?;
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let file_name = std::path::Path::new(remote_path)
        .file_name().map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| remote_path.to_string());

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let remote = remote_path.to_string();
        let local_dir = local_dir.to_string();
        let file_name = file_name.clone();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };
            let host = match host_opt {
                Some(h) => h,
                None => return (host_name.clone(), json!({"ok": false, "error": "host not found"})),
            };
            let local_path = format!("{}/{}/{}", local_dir, host_name, file_name);
            if let Some(parent) = std::path::Path::new(&local_path).parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return (host_name.clone(), json!({"ok": false, "error": format!("mkdir: {e}")}));
                }
            }
            match crate::files::download_file(&host, &remote, &local_path, ca_cert.as_deref(), insecure).await {
                Ok(files) => {
                    if files.len() == 1 {
                        (host_name, json!({
                            "ok": true,
                            "file": {"remote_path": remote, "local_path": files[0].path,
                                      "size": files[0].size, "sha256": files[0].sha256}
                        }))
                    } else {
                        (host_name, json!({
                            "ok": true,
                            "files": files,
                            "total": files.len(),
                        }))
                    }
                }
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    audit(ctx, AuditAction::BatchDownload, "", "", None,
        &format!("hosts:{:?} remote:{}", hosts_arg, remote_path), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}

async fn resize_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let cols = args["cols"].as_u64().unwrap_or(80);
    let rows = args["rows"].as_u64().unwrap_or(24);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "resize_pane", "session_name": session_name, "pane_id": pane_id, "cols": cols, "rows": rows })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ResizePane, host_name, session_name, Some(pane_id), &format!("{}x{}", cols, rows), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn send_text(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let text = args["text"].as_str().context("missing 'text'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "send_text", "session_name": session_name, "pane_id": pane_id, "text": text })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SendText, host_name, session_name, Some(pane_id), text, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn set_pane_title(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let title = args["title"].as_str().context("missing 'title'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "set_pane_title", "session_name": session_name, "pane_id": pane_id, "title": title })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SetPaneTitle, host_name, session_name, Some(pane_id), title, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn find_pane_text(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let pattern = args["pattern"].as_str().context("missing 'pattern'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "find_pane_text", "session_name": session_name, "pane_id": pane_id, "pattern": pattern })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::FindPaneText, host_name, session_name, Some(pane_id), pattern, None, response["found"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn split_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let direction = args["direction"].as_str().unwrap_or("horizontal");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "split_pane", "session_name": session_name, "pane_id": pane_id, "direction": direction })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SplitWindow, host_name, session_name, Some(pane_id), direction, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn close_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "close_pane", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ClosePane, host_name, session_name, Some(pane_id), pane_id, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn close_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "close_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::CloseWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn rename_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let name = args["name"].as_str().context("missing 'name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "rename_window", "session_name": session_name, "window_index": window_index, "name": name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::RenameWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn list_window_panes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "list_window_panes", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ListWindowPanes, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn resize_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let width = args["width"].as_u64()
        .map(|v| u16::try_from(v).context("width must be 0-65535"))
        .transpose()?;
    let height = args["height"].as_u64()
        .map(|v| u16::try_from(v).context("height must be 0-65535"))
        .transpose()?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "resize_window", "session_name": session_name, "window_index": window_index, "width": width, "height": height })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ResizeWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn select_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "select_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SelectWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn select_layout(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let layout = args["layout"].as_str().context("missing 'layout'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "select_layout", "session_name": session_name, "window_index": window_index, "layout": layout })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SelectLayout, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn kill_session(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "kill_session", "session_name": session_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::KillSession, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn pane_info(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "pane_info", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::PaneInfo, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn window_info(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().context("missing 'window_index'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "window_info", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::WindowInfo, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn pane_exists(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "pane_exists", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::PaneExists, host_name, session_name, Some(pane_id), pane_id, None, response["exists"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    ctx: &ToolContext,
    action: AuditAction,
    host: &str,
    session: &str,
    pane_id: Option<&str>,
    detail: &str,
    output_summary: Option<&str>,
    success: bool,
    duration_ms: u64,
    error_message: Option<&str>,
) {
    let agent_name = ctx.agent_name.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let event = AuditEvent {
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent_name,
        host_name: host.to_string(),
        session_name: session.to_string(),
        pane_id: pane_id.map(|s| s.to_string()),
        action,
        detail: detail.to_string(),
        output_summary: output_summary.map(|s| s.to_string()),
        success,
        duration_ms,
        error_message: error_message.map(|s| s.to_string()),
    };
    ctx.audit_db.log(event).await;
}

async fn tunnel_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let local_port = args["local_port"].as_u64().context("missing 'local_port'")? as u16;
    let remote_host = args["remote_host"].as_str().context("missing 'remote_host'")?.to_string();
    let remote_port = args["remote_port"].as_u64().context("missing 'remote_port'")? as u16;
    let local_addr = args["local_addr"].as_str().unwrap_or("127.0.0.1");

    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let result = ctx.tunnel_manager.create(
        host,
        local_addr,
        local_port,
        remote_host.clone(),
        remote_port,
        ctx.ca_cert_path.as_deref(),
        ctx.insecure,
    ).await;

    match result {
        Ok(info) => {
            let detail = format!("{} {}:{} -> {}:{}", info.tunnel_id, local_addr, local_port, remote_host, remote_port);
            audit(ctx, AuditAction::TunnelCreate, host_name, "", None, &detail, None, true, 0, None).await;
            Ok(json!({
                "ok": true,
                "tunnel_id": info.tunnel_id,
                "local_addr": info.local_addr,
                "remote": format!("{}:{}", info.remote_host, info.remote_port),
            }))
        }
        Err(e) => {
            let detail = format!("{}:{} -> {}:{}", local_addr, local_port, remote_host, remote_port);
            audit(ctx, AuditAction::TunnelCreate, host_name, "", None, &detail, None, false, 0, Some(&e.to_string())).await;
            Ok(json!({ "ok": false, "error": e.to_string() }))
        }
    }
}

async fn tunnel_list(ctx: &ToolContext) -> Result<Value> {
    let tunnels = ctx.tunnel_manager.list().await;
    audit(ctx, AuditAction::TunnelList, "", "", None, "", None, true, 0, None).await;
    Ok(json!({
        "ok": true,
        "tunnels": tunnels,
        "count": tunnels.len(),
    }))
}

async fn tunnel_close(ctx: &ToolContext, args: Value) -> Result<Value> {
    let tunnel_id = args["tunnel_id"].as_str().context("missing 'tunnel_id'")?;

    let result = ctx.tunnel_manager.close(tunnel_id).await;
    audit(ctx, AuditAction::TunnelClose, "", "", None, tunnel_id, None, result.is_ok(), 0, None).await;

    match result {
        Ok(()) => Ok(json!({ "ok": true, "closed": tunnel_id })),
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn find_panes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    let mut request = json!({"type": "find_panes"});
    if let Some(v) = args.get("session_name") { request["session_name"] = v.clone(); }
    if let Some(v) = args.get("title") { request["title"] = v.clone(); }
    if let Some(v) = args.get("title_prefix") { request["title_prefix"] = v.clone(); }
    if let Some(v) = args.get("command_contains") { request["command_contains"] = v.clone(); }
    if let Some(v) = args.get("cwd_contains") { request["cwd_contains"] = v.clone(); }
    if let Some(v) = args.get("window_index") { request["window_index"] = v.clone(); }
    if let Some(v) = args.get("running") { request["running"] = v.clone(); }
    if let Some(v) = args.get("exited") { request["exited"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::FindPanes, host_name, "", None, "", None,
        response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn find_sessions(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    let mut request = json!({"type": "find_sessions"});
    if let Some(v) = args.get("name") { request["name"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::FindSessions, host_name, "", None, "", None,
        response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn get_pane_title(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    send_json_frame(&mut tls, &json!({
        "type": "get_pane_title",
        "session_name": session_name,
        "pane_id": pane_id,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::GetPaneTitle, host_name, session_name, Some(pane_id),
        "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn find_text_all(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let pattern = args["pattern"].as_str().context("missing 'pattern'")?;
    let host = ctx.router.get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure,
    ).await?;

    send_json_frame(&mut tls, &json!({
        "type": "find_text_all",
        "session_name": session_name,
        "pane_id": pane_id,
        "pattern": pattern,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::FindTextAll, host_name, session_name, Some(pane_id),
        pattern, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn clear_history(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "clear_history", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ClearHistory, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn list_buffers(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "list_buffers" })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ListBuffers, host_name, "", None, "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn paste_buffer(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let buffer_name = args["buffer_name"].as_str().unwrap_or("");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "paste_buffer", "session_name": session_name, "pane_id": pane_id, "buffer_name": buffer_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::PasteBuffer, host_name, session_name, Some(pane_id), buffer_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn delete_buffer(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let buffer_name = args["buffer_name"].as_str().context("missing 'buffer_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "delete_buffer", "buffer_name": buffer_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::DeleteBuffer, host_name, "", None, buffer_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn split_pane_with(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let direction = args["direction"].as_str().context("missing 'direction'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let shell = args["shell"].as_bool().unwrap_or(true);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    let mut request = json!({
        "type": "split_pane_with",
        "session_name": session_name,
        "pane_id": pane_id,
        "direction": direction,
        "command": command,
        "args": cmd_args,
        "shell": shell,
    });
    if let Some(v) = args.get("cwd") { request["cwd"] = v.clone(); }
    if let Some(v) = args.get("env") { request["env"] = v.clone(); }
    if let Some(v) = args.get("title") { request["title"] = v.clone(); }
    if let Some(v) = args.get("keep_alive_on_exit") { request["keep_alive_on_exit"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SplitPaneWith, host_name, session_name, Some(pane_id), command, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn get_pane_by_title(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let title = args["title"].as_str().context("missing 'title'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({ "type": "get_pane_by_title", "title": title })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::GetPaneByTitle, host_name, "", None, title, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn collect_until_exit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(1048576);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(60000);
    let starting_at = args["starting_at"].as_str().unwrap_or("now");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({
        "type": "collect_until_exit",
        "session_name": session_name,
        "pane_id": pane_id,
        "max_bytes": max_bytes,
        "timeout_ms": timeout_ms,
        "starting_at": starting_at,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::CollectUntilExit, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn break_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().unwrap_or("");
    let destination_window = args["destination_window"].as_u64();
    let detached = args["detached"].as_bool().unwrap_or(false);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    let mut req = json!({
        "type": "break_pane",
        "session_name": session_name,
        "pane_id": pane_id,
        "detached": detached,
    });
    if let Some(dw) = destination_window { req["destination_window"] = json!(dw); }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::BreakPane, host_name, session_name, None, pane_id, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn join_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let source_pane_id = args["source_pane_id"].as_str().context("missing 'source_pane_id'")?;
    let target_pane_id = args["target_pane_id"].as_str().context("missing 'target_pane_id'")?;
    let direction = args["direction"].as_str();
    let size = args["size"].as_u64();
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    let mut req = json!({
        "type": "join_pane",
        "session_name": session_name,
        "source_pane_id": source_pane_id,
        "target_pane_id": target_pane_id,
    });
    if let Some(d) = direction { req["direction"] = json!(d); }
    if let Some(s) = size { req["size"] = json!(s); }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    let detail = format!("{} -> {}", source_pane_id, target_pane_id);
    audit(ctx, AuditAction::JoinPane, host_name, session_name, None, &detail, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn swap_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let source_pane_id = args["source_pane_id"].as_str().context("missing 'source_pane_id'")?;
    let target_pane_id = args["target_pane_id"].as_str().context("missing 'target_pane_id'")?;
    let detached = args["detached"].as_bool().unwrap_or(false);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    send_json_frame(&mut tls, &json!({
        "type": "swap_pane",
        "session_name": session_name,
        "source_pane_id": source_pane_id,
        "target_pane_id": target_pane_id,
        "detached": detached,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    let detail = format!("{} <-> {}", source_pane_id, target_pane_id);
    audit(ctx, AuditAction::SwapPane, host_name, session_name, None, &detail, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn host_capabilities(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let check = args["check"].as_str();
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;
    let mut req = json!({ "type": "capabilities" });
    if let Some(c) = check { req["check"] = json!(c); }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::HostCapabilities, host_name, "", None, "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn capture_region(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let styled = args["styled"].as_bool().unwrap_or(false);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    let mut request = json!({
        "type": "capture_region",
        "session_name": session_name,
        "pane_id": pane_id,
        "styled": styled,
    });
    if let Some(v) = args.get("row") { request["row"] = v.clone(); }
    if let Some(v) = args.get("col") { request["col"] = v.clone(); }
    if let Some(v) = args.get("rows") { request["rows"] = v.clone(); }
    if let Some(v) = args.get("cols") { request["cols"] = v.clone(); }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::CaptureRegion, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn wait_for_bytes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let bytes_b64 = args["bytes"].as_str().context("missing 'bytes'")?;
    let only_new = args["only_new"].as_bool().unwrap_or(false);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({
        "type": "wait_for_bytes",
        "session_name": session_name,
        "pane_id": pane_id,
        "bytes": bytes_b64,
        "only_new": only_new,
        "timeout_ms": timeout_ms,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::WaitForBytes, host_name, session_name, Some(pane_id), bytes_b64, None, response["found"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn wait_stable(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let stable_ms = args["stable_ms"].as_u64().unwrap_or(500);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, ctx.insecure).await?;

    send_json_frame(&mut tls, &json!({
        "type": "wait_stable",
        "session_name": session_name,
        "pane_id": pane_id,
        "stable_ms": stable_ms,
        "timeout_ms": timeout_ms,
    })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::WaitStable, host_name, session_name, Some(pane_id), "", None, response["stable"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn deploy_bridge(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array().context("missing 'hosts'")?
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
    }

    let binary_path = args["binary_path"].as_str().context("missing 'binary_path'")?;
    let user_remote = args["remote_path"].as_str();
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(3) as usize;

    let metadata = tokio::fs::metadata(binary_path).await
        .with_context(|| format!("binary not found at {}", binary_path))?;
    let binary_size = metadata.len();

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let insecure = ctx.insecure;
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
            let binary_path = binary_path.to_string();
        let user_remote = user_remote.map(|s| s.to_string());
            let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };

            let host = match host_opt {
                Some(h) => h,
                None => return (host_name.clone(), json!({
                    "ok": false, "status": "host_not_found",
                    "error": "host not found in registry"
                })),
            };

            let mut stream = match connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                ca_cert.as_deref(), 3, insecure,
            ).await {
                Ok(s) => s,
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "bridge_unreachable",
                    "error": format!("{:#}", e)
                })),
            };

            let session_name = "agent-ops";
            let pane_id = match create_session_inner(&mut stream, session_name).await {
                Ok(resp) => resp.get("pane_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("%0")
                    .to_string(),
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "session_failed",
                    "error": format!("{:#}", e)
                })),
            };

            let exec_result = exec_in_session(&mut stream, session_name, &pane_id,
                "systemctl show rmux-bridge -p ExecStart 2>/dev/null | grep -oP 'path=\\K[^ ;]+' || echo ''",
                10000, 50).await;

            let systemd_path = exec_result.output.trim().to_string();
            if systemd_path.is_empty() {
                return (host_name.clone(), json!({
                    "ok": false, "status": "first_time_deploy",
                    "error": "rmux-bridge service not found — use deploy/install-bridge.sh via SSH"
                }));
            }

            let remote_path = match &user_remote {
                Some(u) => {
                    if u != &systemd_path {
                        return (host_name.clone(), json!({
                            "ok": false, "status": "path_mismatch",
                            "error": format!("specified path '{}' does not match systemd ExecStart '{}'", u, systemd_path)
                        }));
                    }
                    u.clone()
                }
                None => systemd_path,
            };

            let upload_new_path = format!("{}.new", remote_path);
            let upload_result = crate::files::upload_file(
                &host, &binary_path, &upload_new_path,
                ca_cert.as_deref(), insecure,
                crate::files::OverwriteMode::Overwrite, &[],
            ).await;

            match upload_result {
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "upload_failed",
                    "error": format!("{:#}", e)
                })),
                Ok(files) => {
                    let file_failed = files.iter().filter(|f| f.status == "failed").count();
                    if file_failed > 0 {
                        return (host_name.clone(), json!({
                            "ok": false, "status": "upload_failed",
                            "error": format!("{} file(s) failed to upload", file_failed)
                        }));
                    }
                }
            }

            let cmd = format!("chmod +x {new} && nohup sh -c 'sleep 1 && mv {new} {path} && systemctl restart rmux-bridge' > /dev/null 2>&1 & echo deployed",
                new = upload_new_path, path = remote_path);
            let result = exec_in_session(&mut stream, session_name, &pane_id,
                &cmd, 10000, 50).await;

            (host_name.clone(), json!({
                "ok": result.ok && result.error.is_none(),
                "status": if result.ok && result.error.is_none() { "restarted" } else { "exec_failed" },
                "output": result.output,
                "exit_code": result.exit_code,
                "error": result.error,
            }))
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;

    audit(ctx, AuditAction::DeployBridge, "", "", None,
        &format!("hosts:{:?} binary:{}", hosts_arg, binary_path), None,
        failed_count == 0, total_duration_ms, None).await;

    Ok(json!({
        "ok": failed_count == 0,
        "binary": binary_path,
        "binary_size": binary_size,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}
