use agent_ops_core::types::{AuditAction, AuditEvent};
use anyhow::{Context, Result};
use chrono::Utc;
use regex;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::audit;
use crate::router::HostRouter;
use crate::transport::connect_to_bridge_hybrid;

pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: std::sync::Mutex<String>,
}

pub async fn execute_tool(ctx: &ToolContext, tool_name: &str, args: Value) -> Result<Value> {
    match tool_name {
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
        let glob = pattern.replace('*', ".*").replace('?', ".");
        if let Ok(re) = regex::Regex::new(&format!("^{}$", glob)) {
            hosts.retain(|h| re.is_match(&h.name));
        }
    }

    let result: Vec<Value> = hosts
        .iter()
        .map(|h| json!({ "name": h.name, "group": h.group, "tags": h.tags, "labels": h.labels, "bridge_addr": h.bridge_addr }))
        .collect();
    Ok(json!({ "hosts": result, "count": result.len() }))
}

async fn session_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("agent-ops");
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;

    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    let request = json!({ "type": "new_session", "name": session_name, "detached": true });
    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;

    audit(ctx, AuditAction::SessionCreate, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn session_list(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "list_sessions" })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionList, host_name, "", None, "", None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn session_attach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "attach_session", "session_name": session_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionAttach, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn session_detach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "detach_session", "session_name": session_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SessionDetach, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn send_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let keys = args["keys"].as_str().context("missing 'keys'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "send_keys", "session_name": session_name, "pane_id": pane_id, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SendKeys, host_name, session_name, Some(pane_id), keys, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn capture_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "capture_pane", "session_name": session_name, "pane_id": pane_id, "max_lines": max_lines })).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "broadcast_keys", "session_name": session_name, "pane_ids": pane_ids, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::BroadcastKeys, host_name, session_name, None, &format!("{} panes: {}", pane_ids.len(), keys), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn cmd_escape(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "cmd_escape", "host": host_name, "args": cmd_args })).await?;
    let response = recv_json_frame(&mut tls).await?;
    let stdout = response["stdout"].as_str().or_else(|| response["output"].as_str()).unwrap_or("");
    let output_summary = if stdout.len() > 500 { Some(&stdout[..500]) } else if stdout.is_empty() { None } else { Some(stdout) };
    audit(ctx, AuditAction::CmdEscape, host_name, "", None, &format!("{:?}", cmd_args), output_summary, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn respawn_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "respawn_pane", "session_name": session_name, "pane_id": pane_id })).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "split_window", "session_name": session_name, "direction": direction })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SplitWindow, host_name, session_name, None, session_name, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;

    send_json_frame(&mut tls, &json!({ "type": "stream_subscribe", "session_name": session_name, "pane_id": pane_id })).await?;
    recv_json_frame(&mut tls).await
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
    )
    .await;
    audit(ctx, AuditAction::FileDownload, host_name, "", None, remote_path, None, result.is_ok(), 0, None).await;

    match result {
        Ok(file) => Ok(json!({
            "ok": true,
            "file": {
                "uri": format!("file://{}/{}", host_name, remote_path),
                "local_path": local_path,
                "size": file.size,
                "sha256": file.sha256,
            }
        })),
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn send_json_frame<S: tokio::io::AsyncWriteExt + Unpin>(stream: &mut S, value: &Value) -> Result<()> {
    let json_str = serde_json::to_string(value)?;
    let len = json_str.len() as u32;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(json_str.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn recv_json_frame<S: tokio::io::AsyncReadExt + Unpin>(stream: &mut S) -> Result<Value> {
    const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MB
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE);
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let max_lines = args["max_lines"].as_u64().map(|v| v as usize).unwrap_or(200);
    let clear_screen = args["clear_screen"].as_bool().unwrap_or(false);

    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(
        &host.bridge_addr,
        &host.bridge_token,
        ctx.ca_cert_path.as_deref(),
        3,
        false,
    )
    .await?;

    let marker_id = Uuid::new_v4().to_string();
    let marker_id = &marker_id[..3];
    let start_marker = format!("[{}]", marker_id);
    let sentinel_marker = format!("[{} ", marker_id);

    let prefix = if clear_screen { "clear\n" } else { "" };
    let keys = format!("{}echo '{}'\n{}\necho \"{}$?]\"\n", prefix, start_marker, command, sentinel_marker);
    send_json_frame(
        &mut tls,
        &json!({ "type": "send_keys", "session_name": session_name, "pane_id": pane_id, "keys": keys }),
    )
    .await?;
    let send_resp = recv_json_frame(&mut tls).await?;
    if !send_resp["ok"].as_bool().unwrap_or(false) {
        let err = send_resp["error"].as_str().unwrap_or("send_keys failed");
        audit(ctx, AuditAction::Exec, host_name, session_name, Some(pane_id), command, None, false, 0, None).await;
        return Ok(json!({ "ok": false, "output": "", "exit_code": null, "error": err, "duration_ms": 0 }));
    }

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let mut last_text = String::new();

    loop {
        send_json_frame(
            &mut tls,
            &json!({ "type": "capture_pane", "session_name": session_name, "pane_id": pane_id, "max_lines": max_lines }),
        )
        .await?;
        let resp = recv_json_frame(&mut tls).await?;
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
            // 定位起始标记和结束标记，只取中间的本次命令输出
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
                    // 跳过 clear 命令回显
                    if t == "clear" {
                        return false;
                    }
                    // 跳过命令回显行（以 command 开头的行）
                    if t.starts_with(command) || t == command {
                        return false;
                    }
                    // 跳过 echo sentinel 的回显行
                    if t.starts_with("echo") && t.contains(&sentinel_marker) {
                        return false;
                    }
                    // 跳过 sentinel 行 ([caf] / [caf 0])
                    if t.starts_with('[') && t.ends_with(']') && t.len() <= 8 {
                        return false;
                    }
                    true
                })
                .collect();

            let output = output_lines.join("\n").trim().to_string();
            let duration_ms = start.elapsed().as_millis() as u64;

            let output_len = output.len();
            let output_summary = &output[..output_len.min(500)];
            audit(ctx, AuditAction::Exec, host_name, session_name, Some(pane_id), command, Some(output_summary), exit_code == Some(0), duration_ms, None).await;
            return Ok(json!({
                "ok": true,
                "output": output,
                "exit_code": exit_code,
                "duration_ms": duration_ms
            }));
        }

        if std::time::Instant::now() >= deadline {
            let duration_ms = start.elapsed().as_millis() as u64;
            audit(ctx, AuditAction::Exec, host_name, session_name, Some(pane_id), command, None, false, duration_ms, Some("timeout")).await;
            return Ok(json!({
                "ok": false,
                "output": last_text,
                "exit_code": null,
                "error": format!("timeout waiting for sentinel after {}ms", timeout_ms),
                "duration_ms": duration_ms
            }));
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn resize_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let cols = args["cols"].as_u64().unwrap_or(80);
    let rows = args["rows"].as_u64().unwrap_or(24);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "close_pane", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ClosePane, host_name, session_name, Some(pane_id), pane_id, None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn close_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "close_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::CloseWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn rename_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let name = args["name"].as_str().context("missing 'name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "rename_window", "session_name": session_name, "window_index": window_index, "name": name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::RenameWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn list_window_panes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "list_window_panes", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ListWindowPanes, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn resize_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let width = args["width"].as_u64().map(|v| v as u16);
    let height = args["height"].as_u64().map(|v| v as u16);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "resize_window", "session_name": session_name, "window_index": window_index, "width": width, "height": height })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::ResizeWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn select_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "select_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SelectWindow, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn select_layout(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let layout = args["layout"].as_str().context("missing 'layout'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "select_layout", "session_name": session_name, "window_index": window_index, "layout": layout })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::SelectLayout, host_name, session_name, None, &format!("win_{}", window_index), None, response["ok"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

async fn kill_session(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "pane_info", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::PaneInfo, host_name, session_name, Some(pane_id), "", None, response["ok"].as_bool().unwrap_or(true), 0, None).await;
    Ok(response)
}

async fn window_info(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().context("missing 'session_name'")?;
    let window_index = args["window_index"].as_u64().unwrap_or(0);
    let host = ctx.router.get(host_name).with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
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
    let mut tls = connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, ctx.ca_cert_path.as_deref(), 3, false).await?;
    send_json_frame(&mut tls, &json!({ "type": "pane_exists", "session_name": session_name, "pane_id": pane_id })).await?;
    let response = recv_json_frame(&mut tls).await?;
    audit(ctx, AuditAction::PaneExists, host_name, session_name, Some(pane_id), pane_id, None, response["exists"].as_bool().unwrap_or(false), 0, None).await;
    Ok(response)
}

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
