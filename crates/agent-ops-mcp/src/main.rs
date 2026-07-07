#![recursion_limit = "512"]
mod audit;
mod files;
mod router;
mod stream;
mod tools;
mod transport;
mod tunnel;

use clap::Parser;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

use anyhow::Context;

#[derive(Parser)]
#[command(name = "agent-ops-mcp", version, about)]
struct Cli {
    #[arg(long, default_value = "config/hosts.yaml")]
    hosts_file: PathBuf,

    #[arg(long)]
    ca_cert: Option<String>,

    #[arg(long)]
    insecure: bool,

    #[arg(long)]
    audit_db: Option<PathBuf>,

    #[arg(long, default_value = "90")]
    audit_retention_days: u32,

    #[arg(long, default_value = "500")]
    audit_max_size_mb: u64,

    #[arg(long, default_value = "600")]
    audit_cleanup_interval_secs: u64,
}

fn resolve_audit_db_path(custom: Option<PathBuf>) -> PathBuf {
    custom.unwrap_or_else(|| {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".agent-ops");
        std::fs::create_dir_all(&dir).ok();
        dir.join("audit.db")
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Check for audit subcommand (standalone mode, no MCP server)
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "audit" {
        return run_audit_command().await;
    }
    // quinn needs explicit crypto provider in musl builds
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();

    let router = Arc::new(
        router::HostRouter::from_file(&cli.hosts_file)
            .context("failed to load host registry")?,
    );
    tracing::info!("loaded {} hosts", router.len());

    let db_path = resolve_audit_db_path(cli.audit_db);
    let audit_db = Arc::new(audit::AuditDb::open(&db_path)?);
    tracing::info!("audit database: {}", db_path.display());

    let cleanup_db = audit_db.clone();
    let retention_days = cli.audit_retention_days;
    let max_size_mb = cli.audit_max_size_mb;
    let interval = cli.audit_cleanup_interval_secs;
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            timer.tick().await;
            if let Err(e) = cleanup_db.cleanup(retention_days, max_size_mb).await {
                tracing::error!("audit cleanup failed: {e}");
            }
        }
    });

    let ctx = Arc::new(tools::ToolContext {
        router,
        ca_cert_path: cli.ca_cert,
        insecure: cli.insecure,
        audit_db,
        agent_name: std::sync::Mutex::new("unknown".to_string()),
        tunnel_manager: Arc::new(tunnel::TunnelManager::new()),
        stream_manager: Arc::new(stream::StreamManager::new()),
    });

    let tools_definition = serde_json::json!({
        "tools": [
            {
                "name": "agent_ops_usage_rules",
                "description": "⚠️ READ-ONLY: Do NOT call this tool. Role: SRE engineer operating remote Linux hosts. Principles: (1) Verify before destructive operations (2) Follow user's explicit requirements (3) Use default session 'agent-ops' unless specified.",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            },
            {
                "name": "host_list",
                "description": "List all known remote hosts",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            },
            {
                "name": "host_filter",
                "description": "Filter hosts by group, tags, labels, or name glob pattern",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "group": { "type": "string", "description": "Group name, e.g. production" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "label_key": { "type": "string" },
                        "label_value": { "type": "string" },
                        "pattern": { "type": "string", "description": "Hostname glob, e.g. prod-web-*" }
                    }
                }
            },
            {
                "name": "session_create",
                "description": "Create a new terminal session on a host. Returns pane_id.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, default: agent-ops" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "session_list",
                "description": "List all active sessions on a host",
                "inputSchema": {
                    "type": "object",
                    "properties": { "host": { "type": "string", "description": "Hostname, e.g. tf01" } },
                    "required": ["host"]
                }
            },
            {
                "name": "session_attach",
                "description": "Check if a session exists (check-only)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "session_detach",
                "description": "Check if a session exists (check-only)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "send_keys",
                "description": "Send keystrokes to a pane, supporting escape sequences (\\\\n=Enter, \\\\t=Tab, \\\\r=CR, \\\\e=Escape, \\\\x03=Ctrl-C, \\\\xNN=hex). Prefer exec for running commands; prefer send_text for plain text without escape interpretation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "keys": { "type": "string", "description": "Key sequence, e.g. \\n=Enter, \\x03=Ctrl-C" }
                    },
                    "required": ["host", "session_name", "pane_id", "keys"]
                }
            },
            {
                "name": "capture_pane",
                "description": "Capture pane text (default last 200 lines, max_lines=0 for full scrollback). Advanced capture with ansi, start_line, end_line, join_wrapped, preserve_spaces, alternate, buffer_name for fine-grained control.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "max_lines": { "type": "integer", "description": "Default 200, 0=unlimited" },
                        "ansi": { "type": "boolean", "description": "Preserve ANSI escape codes (default: false). When true, text is base64-encoded." },
                        "start_line": { "type": "integer", "description": "Starting line (negative = from end). Overrides max_lines when set." },
                        "end_line": { "type": "integer", "description": "Ending line (negative = from end)" },
                        "join_wrapped": { "type": "boolean", "description": "Join terminal-wrapped lines into single lines (default: false)" },
                        "preserve_spaces": { "type": "boolean", "description": "Preserve trailing spaces (default: false)" },
                        "alternate": { "type": "boolean", "description": "Capture alternate screen (e.g. vim/less). Default: false." },
                        "buffer_name": { "type": "string", "description": "Write capture to a named buffer instead of returning text. Mutually exclusive with other text-return params." }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_for_text",
                "description": "Wait for specific text to appear in a pane (with timeout)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "text": { "type": "string" },
                        "timeout_ms": { "type": "number", "description": "Default 30000" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "spawn_command",
                "description": "Start a new process in a pane (exec, replaces current process). Pane must be idle. For running panes, use exec instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "command": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "shell_command",
                "description": "Run a command via /bin/sh -c in a pane. Pane must be idle. For running panes, use exec instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "command": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "respawn_pane",
                "description": "Respawn a pane process (restart default shell, or launch a custom command). Use when process has exited or shell needs reset. Supports optional command, working directory, environment variables, kill, and keep_alive on exit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "command": { "type": "string", "description": "Replace default shell with this command" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments (used when shell=false)" },
                        "shell": { "type": "boolean", "description": "Run command via /bin/sh -c (default: false, spawn mode)" },
                        "cwd": { "type": "string", "description": "Working directory" },
                        "env": { "type": "object", "description": "Environment variables as KEY:VALUE pairs" },
                        "kill": { "type": "boolean", "description": "Force kill running process before respawn (default: false)" },
                        "keep_alive_on_exit": { "type": "boolean", "description": "Keep pane open after process exits (default: false)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_exit",
                "description": "Wait for a pane process to exit and return exit status",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "timeout_ms": { "type": "number", "description": "Default 30000" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "split_window",
                "description": "Create a new empty window in a session. Use split_pane for pane-level splits.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "direction": { "type": "string", "description": "horizontal or vertical (currently ignored)" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "stream_pane",
                "description": "Blocking read from a pane's output stream. Creates a stream on first call (returns current snapshot + subsequent output), reuses it on later calls (returns only new output). Blocks until data arrives or timeout_ms expires. Use with long-running commands instead of capture_pane polling.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "timeout_ms": { "type": "number", "description": "Blocking timeout in ms (default: 10000)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "file_upload",
                "description": "Upload files/directories to remote host. Auto-creates target dirs. overwrite: overwrite(default)|skip|rename|error. exclude: glob patterns. ⚠️ Do NOT add exclude/overwrite unless user explicitly requests.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "local_path": { "type": "string", "description": "Local file/directory path" },
                        "remote_path": { "type": "string", "description": "Remote destination path" },
                        "overwrite": { "type": "string", "description": "overwrite|skip|rename|error (default: overwrite)" },
                        "exclude": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns, e.g. [\"*.log\"]. Only if user specifies." }
                    },
                    "required": ["host", "local_path", "remote_path"]
                }
            },
            {
                "name": "file_download",
                "description": "Download a file or directory from remote host. Auto-detects path type: single file downloads directly; directory recursively downloads all files preserving structure. Returns size and SHA256 for files, or file list for directories. ⚠️ Do NOT modify paths or add filters unless user explicitly requests.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "remote_path": { "type": "string", "description": "Remote file or directory path to download" },
                        "local_path": { "type": "string", "description": "Local destination path (for directories, this is the root directory)" }
                    },
                    "required": ["host", "remote_path", "local_path"]
                }
            },
            {
                "name": "exec",
                "description": "One-shot command execution: send command → wait for exit → capture output → clean text (default 200 lines, 30s timeout). Automatically clears any unexecuted input before running. Do NOT use shell combiners (&&, ;, |) unless the user explicitly requests it — run commands separately. For self-terminating commands (ls, cat, grep, systemctl, kubectl, curl). NOT for interactive programs (vim, htop) or non-terminating commands (tail -f, ping). Use send_keys + capture_pane for those.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, default: agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "command": { "type": "string", "description": "Shell command, e.g. ls -la" },
                        "timeout_ms": { "type": "number", "description": "Default 30000" },
                        "max_lines": { "type": "integer", "description": "Default 200, 0=unlimited" },
                        "clear_screen": { "type": "boolean", "description": "Clear pane before running" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "split_pane",
                "description": "Split current pane. horizontal=split top/bottom, vertical=split left/right. Returns new pane_id.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "direction": { "type": "string", "description": "horizontal or vertical" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "resize_pane",
                "description": "Resize a pane (cols x rows). Default: 80x24.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "cols": { "type": "integer", "description": "Width, default 80" },
                        "rows": { "type": "integer", "description": "Height, default 24" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "send_text",
                "description": "Send plain text to a pane. Unlike send_keys, does NOT interpret special key sequences. Text stays in terminal input buffer without executing — always follow with exec or send_keys Enter to actually run it. Multiple send_text calls without exec in between will concatenate on the same line.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "set_pane_title",
                "description": "Set pane title",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "title": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id", "title"]
                }
            },
            {
                "name": "find_pane_text",
                "description": "Search pane visible text, return first match position",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "pattern": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id", "pattern"]
                }
            },
            {
                "name": "broadcast_keys",
                "description": "Send same keystrokes to multiple panes simultaneously. Supports special keys like send_keys.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_ids": { "type": "array", "items": { "type": "string" }, "description": "Target pane IDs, e.g. [\"%0\", \"%4\"]" },
                        "keys": { "type": "string" }
                    },
                    "required": ["host", "session_name", "keys"]
                }
            },
            {
                "name": "cmd_escape",
                "description": "Direct rmux CLI access for operations not covered by other tools. Returns stdout, stderr, exit_code.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "rmux CLI arguments" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "close_pane",
                "description": "Close a pane (kill the process). ⚠️ WARNING: NEVER use this unless the user explicitly asks to close the pane.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "rename_window",
                "description": "Rename a window",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "name": { "type": "string" }
                    },
                    "required": ["host", "session_name", "window_index", "name"]
                }
            },
            {
                "name": "list_window_panes",
                "description": "List all panes in a window",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "resize_window",
                "description": "Resize a window (width, height optional)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "select_window",
                "description": "Set a window as active",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "select_layout",
                "description": "Apply window layout: even-horizontal, even-vertical, main-horizontal, main-vertical, tiled",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "layout": { "type": "string" }
                    },
                    "required": ["host", "session_name", "window_index", "layout"]
                }
            },
            {
                "name": "close_window",
                "description": "Close a window (kills all panes within). ⚠️ WARNING: NEVER use this unless the user explicitly asks to close the window.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer", "description": "0-based index" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "kill_session",
                "description": "Destroy an entire terminal session (all windows and panes). ⚠️ WARNING: NEVER use this unless the user explicitly asks to close/kill/destroy the session. Sessions may contain ongoing work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "pane_info",
                "description": "Get pane details: ID, size, command, working directory, tags",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "window_info",
                "description": "Get window details: name, size, index. Use list_window_panes to enumerate panes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "pane_exists",
                "description": "Check if a pane exists",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "batch_exec",
                "description": "Multi-host command execution: sends the same command to all specified hosts concurrently, waits for each to complete (sentinel polling), captures output per host, and returns results keyed by hostname. Default 5 concurrent connections, 200 lines/host, 2min timeout/host. Host-level failures (connection refused, timeout) are marked ok=false but do NOT affect other hosts. Non-zero exit codes set per-host ok=false (but output is always captured — check per-host exit_code). For self-terminating commands only (ls, cat, grep, df, systemctl, kubectl, curl). NOT for interactive programs (vim, htop) or non-terminating commands (tail -f, ping). Uses the agent-ops session default pane (%0) on each host. Use this when you need to run the same command on multiple machines in one round — saves N-1 round trips compared to calling exec per host.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list, e.g. [\"tf01\", \"dns-backup\"]" },
                        "command": { "type": "string", "description": "Command to run on each host" },
                        "timeout_ms": { "type": "number", "description": "Per-host timeout in ms (default: 120000)" },
                        "max_lines": { "type": "integer", "description": "Max output lines per host (default: 200, 0=unlimited)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "command"]
                }
            },
            {
                "name": "batch_upload",
                "description": "Upload a file or directory to multiple hosts concurrently. Each host receives the same file(s) at the specified remote_path. Per-host error isolation. Supports overwrite modes (overwrite|skip|rename|error) and exclude glob patterns. Default 5 concurrent connections. Uses QUIC transport.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list" },
                        "local_path": { "type": "string", "description": "Local file or directory path" },
                        "remote_path": { "type": "string", "description": "Remote destination path" },
                        "overwrite": { "type": "string", "description": "overwrite|skip|rename|error (default: overwrite)" },
                        "exclude": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns to exclude" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "local_path", "remote_path"]
                }
            },
            {
                "name": "batch_download",
                "description": "Download a file from multiple hosts concurrently. Saves to local_dir/<hostname>/<filename> to avoid host-to-host overwrites. ⚠️ Multiple runs to same local_dir WILL overwrite previous downloads. Use different local_dir per run to preserve history. Returns per-host size and SHA256. Per-host error isolation. Default 5 concurrent connections. Uses QUIC transport.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Hostname list" },
                        "remote_path": { "type": "string", "description": "Remote file path to download" },
                        "local_dir": { "type": "string", "description": "Local directory (files saved as <local_dir>/<hostname>/<filename>)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent connections (default: 5, 0=unlimited)" }
                    },
                    "required": ["hosts", "remote_path", "local_dir"]
                }
            },
            {
                "name": "tunnel_create",
                "description": "Create a local port forwarding tunnel to access remote services through QUIC encrypted channel. Opens a local TCP listener that forwards connections to a remote host:port via the bridge. Useful for accessing databases, internal APIs, or services not directly reachable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "local_port": { "type": "integer", "description": "Local port to listen on" },
                        "remote_host": { "type": "string", "description": "Remote target host (can be internal address)" },
                        "remote_port": { "type": "integer", "description": "Remote target port" },
                        "local_addr": { "type": "string", "description": "Local bind address (default: 127.0.0.1)" }
                    },
                    "required": ["host", "local_port", "remote_host", "remote_port"]
                }
            },
            {
                "name": "tunnel_list",
                "description": "List all active port forwarding tunnels with their details.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "tunnel_close",
                "description": "Close an active port forwarding tunnel by its ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tunnel_id": { "type": "string", "description": "Tunnel ID returned by tunnel_create" }
                    },
                    "required": ["tunnel_id"]
                }
            },
            {
                "name": "find_panes",
                "description": "Discover panes across sessions by title, command, working directory, or process state. Returns matching panes with metadata.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Filter by session name" },
                        "title": { "type": "string", "description": "Filter by exact pane title" },
                        "title_prefix": { "type": "string", "description": "Filter by pane title prefix" },
                        "command_contains": { "type": "string", "description": "Filter panes whose command contains this substring" },
                        "cwd_contains": { "type": "string", "description": "Filter panes whose working directory contains this substring" },
                        "window_index": { "type": "integer", "description": "Filter by window index" },
                        "running": { "type": "boolean", "description": "Only show panes with running processes" },
                        "exited": { "type": "boolean", "description": "Only show panes with exited processes" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "find_sessions",
                "description": "Discover sessions by exact name filter. Unlike session_list (which only returns names), find_sessions returns live handles suitable for further operations like querying panes and windows.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "name": { "type": "string", "description": "Exact session name to filter by (optional)" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "get_pane_title",
                "description": "Get the title of a specific pane. Returns the pane title as set by the terminal application.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "find_text_all",
                "description": "Search pane visible text for ALL occurrences of a pattern (including overlapping matches on the same line). Returns all matches with positions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "pattern": { "type": "string", "description": "Text pattern to search for" }
                    },
                    "required": ["host", "session_name", "pane_id", "pattern"]
                }
            },
            {
                "name": "clear_history",
                "description": "Clear pane scrollback history. Unlike exec's clear_screen (which only clears the visible area), this removes all retained output.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "list_buffers",
                "description": "List all paste buffers. Returns buffer name, size in bytes, and content preview for each.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "paste_buffer",
                "description": "⚠️ DANGEROUS: Paste a named buffer into a pane. If the target pane is running a shell (bash/zsh), the buffer content will be executed as commands. This can cause unintended command execution and data loss. BEFORE pasting: (1) use `list_buffers` to check buffer content, (2) print the first 10 lines of buffer content to the user for review, (3) get explicit user approval. If buffer_name is omitted, pastes the most recent buffer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "buffer_name": { "type": "string", "description": "Buffer name to paste (optional, pastes top buffer if omitted)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "delete_buffer",
                "description": "Delete a named paste buffer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "buffer_name": { "type": "string", "description": "Buffer name to delete" }
                    },
                    "required": ["host", "buffer_name"]
                }
            },
            {
                "name": "split_pane_with",
                "description": "Split a pane and run a command in the new pane simultaneously. Use shell flag to control how the command is executed. Supports working directory, environment variables, pane title, and keep-alive on exit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name" },
                        "pane_id": { "type": "string", "description": "Source pane ID to split" },
                        "direction": { "type": "string", "description": "horizontal or vertical" },
                        "command": { "type": "string", "description": "Command to run in the new pane" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments (for shell=false)" },
                        "shell": { "type": "boolean", "description": "Run via /bin/sh -c (default: true)" },
                        "cwd": { "type": "string", "description": "Working directory for the new pane" },
                        "env": { "type": "object", "description": "Environment variables as KEY:VALUE pairs" },
                        "title": { "type": "string", "description": "Title for the new pane" },
                        "keep_alive_on_exit": { "type": "boolean", "description": "Keep pane open after process exits (default: false)" }
                    },
                    "required": ["host", "session_name", "pane_id", "direction", "command"]
                }
            },
            {
                "name": "get_pane_by_title",
                "description": "Find a single pane by exact title. Returns error if zero or multiple panes match. Returns pane metadata.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "title": { "type": "string", "description": "Exact pane title to search for" }
                    },
                    "required": ["host", "title"]
                }
            },
            {
                "name": "collect_until_exit",
                "description": "Collect all pane output from now until the process exits. The pane process MUST already be running — use spawn_command or exec to start it first. More efficient than sentinel polling for commands with large output. Returns collected bytes and exit info.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name" },
                        "pane_id": { "type": "string", "description": "Pane ID" },
                        "max_bytes": { "type": "integer", "description": "Max bytes to collect (default: 1048576 = 1MB)" },
                        "timeout_ms": { "type": "number", "description": "Timeout in ms (default: 60000)" },
                        "starting_at": { "type": "string", "description": "now or oldest (default: now)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "break_pane",
                "description": "Break a pane out of its current window into a new window (or a specified destination window).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname" },
                        "session_name": { "type": "string", "description": "Session name" },
                        "pane_id": { "type": "string", "description": "Pane ID to break (optional, breaks current pane if omitted)" },
                        "destination_window": { "type": "integer", "description": "Target window index (optional, creates new window if omitted)" },
                        "detached": { "type": "boolean", "description": "Detach the pane (default: false)" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "join_pane",
                "description": "Join (move) a pane from one window into another, optionally with direction and size.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname" },
                        "session_name": { "type": "string", "description": "Session name" },
                        "source_pane_id": { "type": "string", "description": "Source pane ID to move" },
                        "target_pane_id": { "type": "string", "description": "Target pane ID to join with" },
                        "direction": { "type": "string", "description": "horizontal or vertical (optional)" },
                        "size": { "type": "integer", "description": "Pane size in cells (optional)" }
                    },
                    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
                }
            },
            {
                "name": "swap_pane",
                "description": "Swap two panes within a session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname" },
                        "session_name": { "type": "string", "description": "Session name" },
                        "source_pane_id": { "type": "string", "description": "First pane to swap" },
                        "target_pane_id": { "type": "string", "description": "Second pane to swap" },
                        "detached": { "type": "boolean", "description": "Detach source pane (default: false)" }
                    },
                    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
                }
            },
            {
                "name": "host_capabilities",
                "description": "Query which features the host rmux daemon supports (e.g. 'web.share', 'sdk.waits', 'stream.control'). Use before attempting advanced operations to verify compatibility. Optionally check for a specific capability.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "check": { "type": "string", "description": "Specific capability to check for (optional)" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "capture_region",
                "description": "Capture a rectangular region of a pane, or capture the full pane screenshot when no coordinates are specified. Supports plain text or styled (color markup) output.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "row": { "type": "integer", "description": "Top row (0-based). Omit all coords for full screenshot." },
                        "col": { "type": "integer", "description": "Left column (0-based)" },
                        "rows": { "type": "integer", "description": "Height in rows" },
                        "cols": { "type": "integer", "description": "Width in columns" },
                        "styled": { "type": "boolean", "description": "Preserve style/color markup (default: false, plain text)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_for_bytes",
                "description": "Wait for specific raw bytes to appear in the pane output stream. Unlike wait_for_text (which only matches visible text), this matches the raw output including ANSI sequences and control characters.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "bytes": { "type": "string", "description": "Raw bytes to wait for (as base64-encoded string)" },
                        "only_new": { "type": "boolean", "description": "Only match data appearing after this call (skip history, default: false)" },
                        "timeout_ms": { "type": "number", "description": "Default 30000" }
                    },
                    "required": ["host", "session_name", "pane_id", "bytes"]
                }
            },
            {
                "name": "wait_stable",
                "description": "Wait until the pane output has been stable (no changes) for a given duration. Useful after sending commands to ensure terminal rendering is complete before capturing output.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "stable_ms": { "type": "number", "description": "Duration of stability required in ms (default: 500)" },
                        "timeout_ms": { "type": "number", "description": "Max total wait time in ms (default: 30000)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "deploy_bridge",
                "description": "Deploy a compiled rmux-bridge binary to remote hosts and restart the service. For upgrade deployments only — target hosts must already have rmux-bridge running. First-time deployments must use deploy/install-bridge.sh via SSH. Internal steps: upload binary → set permissions → replace existing → restart service via nohup.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Target hostnames (must already have bridge running)" },
                        "binary_path": { "type": "string", "description": "Local path to compiled rmux-bridge binary" },
                        "remote_path": { "type": "string", "description": "Remote binary path (auto-detected from systemd ExecStart if omitted)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent deployments (default: 3)" }
                    },
                    "required": ["hosts", "binary_path"]
                }
            }
        ]
    });

    tracing::info!("agent-ops-mcp server starting (stdio mode)");
    run_mcp_stdio_loop(ctx, tools_definition).await
}

async fn run_mcp_stdio_loop(ctx: Arc<tools::ToolContext>, tools_def: Value) -> anyhow::Result<()> {
    let stdin = BufReader::new(stdin());
    let mut stdout = stdout();
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json_rpc_error(None, -32700, &format!("Parse error: {e}"));
                stdout.write_all(err.to_string().as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = request.get("id").cloned();

        let response = match method {
            "tools/list" => json_rpc_response(id, &tools_def),
            "tools/call" => {
                let tool_name = request["params"]["name"].as_str().unwrap_or("");
                let args = request["params"]["arguments"].clone();
                match tools::execute_tool(&ctx, tool_name, args).await {
                    Ok(result) => json_rpc_response(id, &json!({
                        "content": [{ "type": "text", "text": result.to_string() }]
                    })),
                    Err(e) => json_rpc_error(id, -32000, &format!("Tool error: {e}")),
                }
            }
            "initialize" => {
                let agent_name = request["params"]["clientInfo"]["name"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                *ctx.agent_name.lock().unwrap_or_else(|e| e.into_inner()) = agent_name;
                json_rpc_response(id, &json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agent-ops-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "You are an AI agent managing remote Linux hosts via agent-ops. 你是通过 agent-ops 运维远程主机的 AI Agent。\n\n## Rules\n1. If a host is in the agent-ops registry (`host_list`), ALL operations MUST use agent-ops tools. NEVER run ssh/scp/rsync directly.\n2. Default session: `\"agent-ops\"`. Always `session_attach` first; `session_create` if not found.\n3. File transfer: `file_upload` / `file_download`. Commands: `exec` for one-shot (auto-waits, default 200 lines / 30s timeout, set `max_lines=0` for everything), `send_keys` for interactive programs.\n4. Use `wait_for_text` to block until a pattern appears — do NOT poll `capture_pane` in a loop.\n5. For long-running commands (tail -f, builds), use `stream_pane` for incremental output instead of polling `capture_pane`. `session_attach`/`session_detach` only check existence, they don't attach/detach.\n\n## Workflow\n`host_list` → `session_attach host=<h> session_name=\"agent-ops\"` (or `session_create`) → `exec`/`send_keys` → `capture_pane`/`wait_for_text` → `close_pane` to clean up.\n- Default pane after session_create: `%0`.\n- `exec` supports `clear_screen: true` and `timeout_ms` for long commands.\n- After closing a pane: `respawn_pane` to restart the shell.\n- `cmd_escape` for direct rmux CLI access (advanced)."
                }))
            }
            _ => json_rpc_error(id, -32601, &format!("Method not found: {method}")),
        };

        stdout.write_all(response.to_string().as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn run_audit_command() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    struct AuditCli {
        #[command(subcommand)]
        command: AuditCommand,
    }

    #[derive(clap::Subcommand)]
    enum AuditCommand {
        Query {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            host: Option<String>,
            #[arg(long)]
            action: Option<String>,
            #[arg(long)]
            agent: Option<String>,
            #[arg(long)]
            since: Option<String>,
            #[arg(long)]
            until: Option<String>,
            #[arg(long)]
            success: Option<bool>,
            #[arg(long, default_value = "50")]
            limit: u32,
            #[arg(long, default_value = "table")]
            format: String,
        },
        Stats {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            since: Option<String>,
        },
        Cleanup {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            older_than: Option<u32>,
            #[arg(long)]
            max_size: Option<u64>,
        },
    }

    let cli = AuditCli::parse_from(
        std::iter::once("agent-ops-mcp".to_string())
            .chain(std::env::args().skip(2)),
    );
    match cli.command {
        AuditCommand::Query { db, host, action, agent, since, until, success, limit, format } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let fmt = match format.as_str() {
                "json" => audit::query::OutputFormat::Json,
                "jsonl" => audit::query::OutputFormat::Jsonl,
                _ => audit::query::OutputFormat::Table,
            };
            let params = audit::query::QueryParams {
                host, action, agent, since, until, success, limit: Some(limit),
            };
            let result = audit_db.query(params, fmt).await?;
            println!("{}", result);
        }
        AuditCommand::Stats { db, since } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let result = audit_db.stats(since).await?;
            println!("{}", result);
        }
        AuditCommand::Cleanup { db, older_than, max_size } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let days = older_than.unwrap_or(90);
            let size = max_size.unwrap_or(500);
            audit_db.cleanup(days, size).await?;
            println!("Cleanup completed.");
        }
    }
    Ok(())
}

fn json_rpc_response(id: Option<Value>, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn json_rpc_error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
