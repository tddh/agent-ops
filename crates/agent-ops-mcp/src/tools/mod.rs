use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::audit;
use crate::router::HostRouter;
use crate::stream::StreamManager;
use crate::tunnel::TunnelManager;

mod batch;
mod buffer;
mod common;
mod deploy;
mod discovery;
mod exec;
mod file;
mod output;
mod pane;
mod session;
mod tunnel;
mod window;

// Re-export audit for all sub-modules
pub(crate) use deploy::audit;

pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: String,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: std::sync::Mutex<String>,
    pub tunnel_manager: Arc<TunnelManager>,
    pub stream_manager: Arc<StreamManager>,
}

pub async fn execute_tool(ctx: &ToolContext, tool_name: &str, args: Value) -> Result<Value> {
    match tool_name {
        "agent_ops_usage_rules" => Ok(json!({})),
        "host_list" => discovery::host_list(ctx).await,
        "host_filter" => discovery::host_filter(ctx, args).await,
        "session_create" => session::session_create(ctx, args).await,
        "session_list" => session::session_list(ctx, args).await,
        "session_attach" => session::session_attach(ctx, args).await,
        "session_detach" => session::session_detach(ctx, args).await,
        "send_keys" => pane::send_keys(ctx, args).await,
        "capture_pane" => pane::capture_pane(ctx, args).await,
        "wait_for_text" => output::wait_for_text(ctx, args).await,
        "wait_exit" => output::wait_exit(ctx, args).await,
        "spawn_command" => exec::spawn_command(ctx, args).await,
        "shell_command" => exec::shell_command(ctx, args).await,
        "respawn_pane" => session::respawn_pane(ctx, args).await,
        "broadcast_keys" => exec::broadcast_keys(ctx, args).await,
        "cmd_escape" => exec::cmd_escape(ctx, args).await,
        "split_window" => window::split_window(ctx, args).await,
        "stream_pane" => window::stream_pane(ctx, args).await,
        "file_upload" => file::file_upload(ctx, args).await,
        "file_download" => file::file_download(ctx, args).await,
        "exec" => exec::exec(ctx, args).await,
        "close_pane" => pane::close_pane(ctx, args).await,
        "split_pane" => pane::split_pane(ctx, args).await,
        "resize_pane" => pane::resize_pane(ctx, args).await,
        "send_text" => pane::send_text(ctx, args).await,
        "set_pane_title" => pane::set_pane_title(ctx, args).await,
        "find_pane_text" => pane::find_pane_text(ctx, args).await,
        "close_window" => window::close_window(ctx, args).await,
        "kill_session" => session::kill_session(ctx, args).await,
        "rename_window" => window::rename_window(ctx, args).await,
        "list_window_panes" => window::list_window_panes(ctx, args).await,
        "resize_window" => window::resize_window(ctx, args).await,
        "select_window" => window::select_window(ctx, args).await,
        "select_layout" => window::select_layout(ctx, args).await,
        "pane_info" => window::pane_info(ctx, args).await,
        "window_info" => window::window_info(ctx, args).await,
        "pane_exists" => window::pane_exists(ctx, args).await,
        "batch_exec" => batch::batch_exec(ctx, args).await,
        "batch_upload" => batch::batch_upload(ctx, args).await,
        "batch_download" => batch::batch_download(ctx, args).await,
        "tunnel_create" => tunnel::tunnel_create(ctx, args).await,
        "tunnel_list" => tunnel::tunnel_list(ctx).await,
        "tunnel_close" => tunnel::tunnel_close(ctx, args).await,
        "find_panes" => discovery::find_panes(ctx, args).await,
        "find_sessions" => discovery::find_sessions(ctx, args).await,
        "get_pane_title" => pane::get_pane_title(ctx, args).await,
        "find_text_all" => output::find_text_all(ctx, args).await,
        "clear_history" => pane::clear_history(ctx, args).await,
        "list_buffers" => buffer::list_buffers(ctx, args).await,
        "paste_buffer" => buffer::paste_buffer(ctx, args).await,
        "delete_buffer" => buffer::delete_buffer(ctx, args).await,
        "split_pane_with" => pane::split_pane_with(ctx, args).await,
        "get_pane_by_title" => pane::get_pane_by_title(ctx, args).await,
        "collect_until_exit" => exec::collect_until_exit(ctx, args).await,
        "break_pane" => pane::break_pane(ctx, args).await,
        "join_pane" => pane::join_pane(ctx, args).await,
        "swap_pane" => pane::swap_pane(ctx, args).await,
        "host_capabilities" => discovery::host_capabilities(ctx, args).await,
        "capture_region" => pane::capture_region(ctx, args).await,
        "wait_for_bytes" => output::wait_for_bytes(ctx, args).await,
        "wait_stable" => output::wait_stable(ctx, args).await,
        "deploy_bridge" => deploy::deploy_bridge(ctx, args).await,
        "reload_config" => session::reload_config(ctx).await,
        _ => anyhow::bail!("unknown tool: {}", tool_name),
    }
}
