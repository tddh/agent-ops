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
                "description": "List all registered remote hosts from the host registry (hosts.yaml). Returns an array of host objects with name, group, tags, labels, and bridge_addr. Use this to discover available hosts before performing operations. This is typically the first tool called in any workflow.",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            },
            {
                "name": "host_filter",
                "description": "Filter hosts from the registry by group, tags, labels, or name glob pattern. All filters are ANDed together. Use this to target specific subsets of hosts for batch operations. Returns matching hosts with full metadata. Example: filter by group='production' and tags=['web'] to get all production web servers.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "group": { "type": "string", "description": "Group name, e.g. production" },
                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags to match (all must be present)" },
                        "label_key": { "type": "string", "description": "Label key to filter by" },
                        "label_value": { "type": "string", "description": "Label value to match (used with label_key)" },
                        "pattern": { "type": "string", "description": "Hostname glob pattern, e.g. prod-web-*, supports * and ? wildcards" }
                    }
                }
            },
            {
                "name": "session_create",
                "description": "Create a new detached terminal session on a remote host. Returns the session info including the initial pane_id (typically %0). Sessions persist across disconnects. If a session with the same name already exists, it will return an error — use session_attach to check first. Default session name is 'agent-ops'. Use this when you need a fresh session or the default doesn't exist yet.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name (default: 'agent-ops'). Must be unique per host." }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "session_list",
                "description": "List all active terminal sessions on a remote host. Returns session names and metadata. Use this to discover existing sessions before creating new ones or to verify session state. Sessions are persistent and survive disconnects.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "host": { "type": "string", "description": "Hostname, e.g. tf01" } },
                    "required": ["host"]
                }
            },
            {
                "name": "session_attach",
                "description": "Check if a session exists on the remote host. Returns ok=true if the session exists, ok=false otherwise. This is a read-only check — it does NOT attach to the session or modify its state. Use this before session_create to avoid 'session already exists' errors, or to verify a session is still active. Typical workflow: session_attach → if not found → session_create.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name to check, e.g. 'agent-ops'" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "session_detach",
                "description": "Check if a session exists on the remote host. Functionally identical to session_attach — both are read-only existence checks. Returns ok=true if the session exists, ok=false otherwise. The name 'detach' is historical; this does NOT detach from a session. Use session_attach or session_detach interchangeably for existence checks.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name to check, e.g. 'agent-ops'" }
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
                "description": "Block until specific text appears in the pane's visible output, or timeout expires. Polls the pane content periodically and returns as soon as the text is found. Returns found=true if text appeared, found=false on timeout. Use this instead of polling capture_pane in a loop. Ideal for waiting for command prompts, completion messages, or error indicators. Default timeout is 30 seconds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "text": { "type": "string", "description": "Text pattern to wait for (exact match, not regex)" },
                        "timeout_ms": { "type": "number", "description": "Maximum wait time in milliseconds (default: 30000)" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "spawn_command",
                "description": "Start a new process in a pane using exec semantics (replaces the current shell process). The pane MUST be idle (no running process) — if a process is running, use exec or respawn_pane with kill=true instead. Use this for long-running processes that need to run in the foreground (e.g., top, htop, vim, tail -f). Unlike exec, this does NOT wait for completion or capture output — use stream_pane or capture_pane to monitor output. Args are passed directly to the command without shell interpretation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "command": { "type": "string", "description": "Command to execute (e.g., 'top', 'vim', 'tail')" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments (e.g., ['-f', '/var/log/syslog'])" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "shell_command",
                "description": "Run a command via /bin/sh -c in a pane, replacing the current shell process. The pane MUST be idle (no running process). Unlike spawn_command, this interprets the command through a shell, so you can use shell features like pipes, redirects, and variable expansion. Use this for complex shell one-liners. Unlike exec, this does NOT wait for completion or capture output — use stream_pane or capture_pane to monitor. For simple commands that need output captured, prefer exec.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "command": { "type": "string", "description": "Shell command to execute (e.g., 'ls -la | grep foo > /tmp/out')" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "respawn_pane",
                "description": "Respawn a pane's process — restart the default shell or launch a custom command. Use this when a process has exited and you want to reuse the pane, when the shell needs a reset, or when you want to replace the running process. If the pane has a running process, set kill=true to force-kill it first (otherwise respawn will fail). Supports custom command, working directory, environment variables, and keep_alive_on_exit to prevent the pane from closing when the process exits. Without a command parameter, restarts the default shell.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "command": { "type": "string", "description": "Replace default shell with this command (optional)" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments (used when shell=false)" },
                        "shell": { "type": "boolean", "description": "Run command via /bin/sh -c (default: false, spawn mode)" },
                        "cwd": { "type": "string", "description": "Working directory for the new process" },
                        "env": { "type": "object", "description": "Environment variables as KEY:VALUE pairs" },
                        "kill": { "type": "boolean", "description": "Force kill running process before respawn (default: false)" },
                        "keep_alive_on_exit": { "type": "boolean", "description": "Keep pane open after process exits (default: false)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_exit",
                "description": "Wait for the process running in a pane to exit and return its exit status. Blocks until the process terminates or timeout expires. Returns ok=true if process exited within timeout, ok=false on timeout. Use this after spawn_command or shell_command to wait for completion. Default timeout is 30 seconds. Note: exec already waits for exit internally — you don't need wait_exit after exec.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "timeout_ms": { "type": "number", "description": "Maximum wait time in milliseconds (default: 30000)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "split_window",
                "description": "Create a new empty window in a session. The new window contains a single pane running the default shell. Use this to create separate workspaces within a session (like browser tabs). For splitting an existing pane into multiple panes, use split_pane instead. The direction parameter is currently ignored. Returns the new window index.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "direction": { "type": "string", "description": "horizontal or vertical (currently ignored, reserved for future use)" }
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
                "description": "Split an existing pane into two panes. horizontal direction creates top/bottom panes, vertical direction creates left/right panes. The new pane runs a default shell. Returns the new pane_id. Use this to create multiple panes within a window for parallel work. For creating a new window (separate workspace), use split_window. For splitting and immediately running a command in the new pane, use split_pane_with.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID to split, e.g. %0" },
                        "direction": { "type": "string", "description": "horizontal (top/bottom) or vertical (left/right). Default: horizontal" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "resize_pane",
                "description": "Resize a pane to the specified dimensions (columns x rows). Default size is 80x24. Use this to adjust pane size for better visibility or to fit specific output. Note: in rmux, pane sizes are constrained by the window size and other panes — the actual size may differ from requested if constraints prevent exact sizing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "cols": { "type": "integer", "description": "Width in columns (default: 80)" },
                        "rows": { "type": "integer", "description": "Height in rows (default: 24)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "send_text",
                "description": "Send plain text to a pane's input buffer WITHOUT interpreting escape sequences. Unlike send_keys, backslashes and special characters are sent literally. The text stays in the terminal input buffer and is NOT executed — you must follow with exec or send_keys '\\n' to run it. Multiple send_text calls without execution in between will concatenate on the same input line. Use this when you need to send text that contains escape-like sequences (e.g., '\\n', '\\t') as literal characters.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "text": { "type": "string", "description": "Plain text to send (no escape interpretation)" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "set_pane_title",
                "description": "Set the title of a pane. The title is displayed in the pane's status bar and can be used to identify panes. Use get_pane_by_title or find_panes to locate panes by title later. Titles are useful for organizing multiple panes in complex workflows.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "title": { "type": "string", "description": "Title to set (e.g., 'web-server', 'db-monitor')" }
                    },
                    "required": ["host", "session_name", "pane_id", "title"]
                }
            },
            {
                "name": "find_pane_text",
                "description": "Search the pane's visible text for the first occurrence of a pattern. Returns found=true with the position (row, column) if found, found=false otherwise. Only searches the currently visible area (not scrollback). Use this to quickly check if specific text is visible on screen. For searching all occurrences, use find_text_all. For searching scrollback history, use capture_pane with max_lines=0 and search the result.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "pattern": { "type": "string", "description": "Text pattern to search for (exact match, not regex)" }
                    },
                    "required": ["host", "session_name", "pane_id", "pattern"]
                }
            },
            {
                "name": "broadcast_keys",
                "description": "Send the same keystrokes to multiple panes simultaneously. All target panes receive the input at the same time. Supports escape sequences like send_keys (\\n=Enter, \\t=Tab, \\x03=Ctrl-C, etc.). Use this to execute the same command across multiple panes in parallel (e.g., running the same command on multiple servers). Note: pane_ids parameter is optional — if omitted, sends to all panes in the window.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_ids": { "type": "array", "items": { "type": "string" }, "description": "Target pane IDs (e.g., ['%0', '%1']). If omitted, broadcasts to all panes in the window." },
                        "keys": { "type": "string", "description": "Key sequence to send (supports \\n, \\t, \\x03, \\xNN, etc.)" }
                    },
                    "required": ["host", "session_name", "keys"]
                }
            },
            {
                "name": "cmd_escape",
                "description": "Execute rmux CLI commands directly on the remote host, bypassing the standard tool interface. Use this for advanced operations not covered by other tools (e.g., custom rmux commands, debugging). Returns stdout, stderr, and exit_code. This is an escape hatch — prefer standard tools (exec, send_keys, etc.) when possible. Requires rmux to be installed on the remote host.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "rmux CLI arguments (e.g., ['list-sessions'], ['display-message', '-p', '#{pane_id}'])" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "close_pane",
                "description": "Close a pane and kill its running process. The pane is permanently removed from the window. ⚠️ WARNING: NEVER use this unless the user explicitly asks to close/kill/destroy the pane. Closing a pane will terminate any running process and discard all output. If you need to restart the process, use respawn_pane instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID to close, e.g. %0" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "rename_window",
                "description": "Rename a window to the specified name. The window name is displayed in the window status bar and can be used to identify windows. Use window_info to get the current window index, or list_window_panes to enumerate windows. Window names are useful for organizing multiple workspaces within a session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index (0-based). Use window_info or list_window_panes to find the index." },
                        "name": { "type": "string", "description": "New window name (e.g., 'web-server', 'database')" }
                    },
                    "required": ["host", "session_name", "window_index", "name"]
                }
            },
            {
                "name": "list_window_panes",
                "description": "List all panes in a specific window. Returns an array of pane objects with pane_id, size, title, command, and working directory. Use this to discover pane IDs for a window, or to verify pane state. Window index is 0-based. Use window_info to get window metadata.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index (0-based)" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "resize_window",
                "description": "Resize a window to the specified dimensions (width x height in cells). Both width and height are optional — if omitted, the window uses the default size. Note: window size affects all panes within it. Use resize_pane to adjust individual panes within a window.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index (0-based)" },
                        "width": { "type": "integer", "description": "Window width in columns (optional)" },
                        "height": { "type": "integer", "description": "Window height in rows (optional)" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "select_window",
                "description": "Set a window as the active (visible) window in a session. Only one window can be active at a time. Use this to switch between different workspaces within a session. Window index is 0-based. Use window_info or list_window_panes to discover window indices.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index to activate (0-based)" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "select_layout",
                "description": "Apply a predefined layout to a window, automatically arranging all panes. Available layouts: 'even-horizontal' (panes side by side), 'even-vertical' (panes stacked), 'main-horizontal' (large pane on top, others below), 'main-vertical' (large pane on left, others right), 'tiled' (panes in a grid). Use this to quickly reorganize panes without manual resizing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index (0-based)" },
                        "layout": { "type": "string", "description": "Layout name: even-horizontal, even-vertical, main-horizontal, main-vertical, or tiled" }
                    },
                    "required": ["host", "session_name", "window_index", "layout"]
                }
            },
            {
                "name": "close_window",
                "description": "Close a window and kill all panes within it. All running processes in the window's panes will be terminated. The window is permanently removed from the session. ⚠️ WARNING: NEVER use this unless the user explicitly asks to close/kill/destroy the window. Use list_window_panes to verify window contents before closing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index to close (0-based). Use window_info or list_window_panes to find the index." }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "kill_session",
                "description": "Destroy an entire terminal session — all windows, panes, and running processes are terminated. The session is permanently removed from the host. ⚠️ WARNING: NEVER use this unless the user explicitly asks to close/kill/destroy the session. Sessions may contain ongoing work, unsaved data, or long-running processes. Use session_list or find_sessions to verify session contents before destroying.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name to destroy (e.g., 'agent-ops')" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "pane_info",
                "description": "Get detailed information about a pane. Returns pane_id, size (cols x rows), current command, working directory, title, and tags. Use this to verify pane state, check what process is running, or get the working directory. For listing all panes in a window, use list_window_panes.",
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
                "name": "window_info",
                "description": "Get detailed information about a window. Returns window name, size (width x height), index, and active pane. Use this to verify window state or get metadata. To list all panes within the window, use list_window_panes. To list all windows in a session, use find_sessions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "window_index": { "type": "integer", "description": "Window index (0-based)" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "pane_exists",
                "description": "Check if a pane exists in a session. Returns ok=true if the pane exists, ok=false otherwise. Use this to verify pane state before performing operations. Pane IDs are typically %0, %1, %2, etc. Use list_window_panes to discover valid pane IDs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID to check, e.g. %0" }
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
                "description": "Create a local port forwarding tunnel to access remote services through an encrypted QUIC channel. Opens a local TCP listener on the specified port that forwards all connections to a remote host:port via the bridge. The remote_host can be an internal address (e.g., 127.0.0.1, 10.x.x.x) not directly reachable from your machine. Use this to access databases, internal APIs, admin panels, or any TCP service running on remote hosts. Returns a tunnel_id that can be used with tunnel_close. Tunnels persist until explicitly closed or the MCP server restarts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname of the bridge to connect through, e.g. tf01" },
                        "local_port": { "type": "integer", "description": "Local port to listen on (e.g., 5432 for PostgreSQL)" },
                        "remote_host": { "type": "string", "description": "Remote target host (can be internal address like 127.0.0.1 or 10.x.x.x)" },
                        "remote_port": { "type": "integer", "description": "Remote target port (e.g., 5432 for PostgreSQL)" },
                        "local_addr": { "type": "string", "description": "Local bind address (default: 127.0.0.1, use 0.0.0.0 to listen on all interfaces)" }
                    },
                    "required": ["host", "local_port", "remote_host", "remote_port"]
                }
            },
            {
                "name": "tunnel_list",
                "description": "List all active port forwarding tunnels. Returns an array of tunnel objects with tunnel_id, local address/port, remote host/port, and status. Use this to discover existing tunnels before creating new ones, or to verify tunnel state. Tunnels persist until explicitly closed or the MCP server restarts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "tunnel_close",
                "description": "Close an active port forwarding tunnel by its ID. The tunnel stops accepting new connections and existing connections are terminated. Use tunnel_list to discover tunnel IDs. Once closed, the tunnel cannot be reopened — create a new tunnel with tunnel_create if needed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tunnel_id": { "type": "string", "description": "Tunnel ID returned by tunnel_create (e.g., 'tunnel_abc123')" }
                    },
                    "required": ["tunnel_id"]
                }
            },
            {
                "name": "find_panes",
                "description": "Discover panes across sessions by various criteria. All filters are ANDed together. Returns matching panes with full metadata (pane_id, session, window, title, command, cwd, state). Use this to locate specific panes in complex setups. Examples: find panes running 'vim', find panes in '/var/log' directory, find exited panes that need cleanup. For finding a single pane by exact title, use get_pane_by_title.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Filter by session name (exact match)" },
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
                "description": "Discover sessions on a remote host with detailed metadata. Unlike session_list (which only returns session names), find_sessions returns session objects with windows, panes, and state information. Optionally filter by exact session name. Use this to explore the full session structure or to verify session state. For a simple list of session names, use session_list.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "name": { "type": "string", "description": "Exact session name to filter by (optional, returns all sessions if omitted)" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "get_pane_title",
                "description": "Get the title of a specific pane. Returns the pane title as set by set_pane_title or by the terminal application (e.g., vim sets its own title). Pane titles are useful for identifying panes in complex setups. Use get_pane_by_title to find a pane by its title.",
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
                "description": "Search the pane's visible text for ALL occurrences of a pattern, including overlapping matches on the same line. Returns an array of matches with row and column positions. Only searches the currently visible area (not scrollback). Use this when you need to find all instances of a pattern (e.g., counting errors, locating all occurrences of a keyword). For finding just the first match, use find_pane_text.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "pattern": { "type": "string", "description": "Text pattern to search for (exact match, not regex)" }
                    },
                    "required": ["host", "session_name", "pane_id", "pattern"]
                }
            },
            {
                "name": "clear_history",
                "description": "Clear the pane's scrollback history, removing all retained output above the visible area. Unlike exec's clear_screen parameter (which only clears the visible area and can be undone by scrolling up), this permanently removes all scrollback content. Use this to free memory or start with a clean slate. The visible area is not affected.",
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
                "description": "List all paste buffers on the remote host. Returns an array of buffer objects with buffer name, size in bytes, and a content preview (first few lines). Buffers are used to store text that can be pasted into panes later. Use this before paste_buffer to verify buffer content and avoid unintended command execution.",
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
                "description": "Delete a named paste buffer. The buffer is permanently removed and cannot be recovered. Use list_buffers to discover buffer names. If the buffer doesn't exist, the operation returns an error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "buffer_name": { "type": "string", "description": "Buffer name to delete (e.g., 'buffer0', 'my-buffer')" }
                    },
                    "required": ["host", "buffer_name"]
                }
            },
            {
                "name": "split_pane_with",
                "description": "Split an existing pane and immediately run a command in the new pane — combines split_pane and spawn_command in one operation. The new pane is created and the command starts executing right away. Use the shell flag to control execution: shell=true (default) interprets the command through /bin/sh -c (supports pipes, redirects); shell=false passes args directly to the command. Supports custom working directory, environment variables, pane title, and keep_alive_on_exit to prevent the pane from closing when the command finishes. Returns the new pane_id. Use this for parallel workflows where you want to start multiple commands simultaneously.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Source pane ID to split, e.g. %0" },
                        "direction": { "type": "string", "description": "Split direction: horizontal (top/bottom) or vertical (left/right)" },
                        "command": { "type": "string", "description": "Command to run in the new pane (e.g., 'tail -f /var/log/syslog')" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments (used when shell=false)" },
                        "shell": { "type": "boolean", "description": "Run command via /bin/sh -c (default: true). Set false for direct exec without shell interpretation." },
                        "cwd": { "type": "string", "description": "Working directory for the new pane" },
                        "env": { "type": "object", "description": "Environment variables as KEY:VALUE pairs" },
                        "title": { "type": "string", "description": "Title for the new pane (useful for identification)" },
                        "keep_alive_on_exit": { "type": "boolean", "description": "Keep pane open after process exits (default: false)" }
                    },
                    "required": ["host", "session_name", "pane_id", "direction", "command"]
                }
            },
            {
                "name": "get_pane_by_title",
                "description": "Find a single pane by its exact title. Returns the pane metadata if exactly one pane matches, or an error if zero or multiple panes match. Use this when you know the exact title and expect a unique match. For more flexible searching (prefix, partial match), use find_panes. Pane titles are set by set_pane_title or by the terminal application.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "title": { "type": "string", "description": "Exact pane title to search for (case-sensitive)" }
                    },
                    "required": ["host", "title"]
                }
            },
            {
                "name": "collect_until_exit",
                "description": "Collect all pane output from now until the process exits. The pane process MUST already be running — use spawn_command or shell_command to start it first. More efficient than sentinel polling for commands with large output, as it streams directly without repeated capture_pane calls. Returns collected bytes and exit info. Default max is 1MB, default timeout is 60s. Use starting_at='oldest' to include scrollback history.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "max_bytes": { "type": "integer", "description": "Maximum bytes to collect (default: 1048576 = 1MB)" },
                        "timeout_ms": { "type": "number", "description": "Timeout in milliseconds (default: 60000)" },
                        "starting_at": { "type": "string", "description": "Where to start collecting: 'now' (default) or 'oldest' (includes scrollback)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "break_pane",
                "description": "Break a pane out of its current window and move it to a new window (or a specified destination window). The pane retains its state and running process. Use this to reorganize panes across windows. If destination_window is omitted, a new window is created. If pane_id is omitted, the current active pane is broken out.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID to break out (optional, breaks current pane if omitted)" },
                        "destination_window": { "type": "integer", "description": "Target window index (optional, creates new window if omitted)" },
                        "detached": { "type": "boolean", "description": "Detach the pane (default: false)" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "join_pane",
                "description": "Move a pane from one window into another window, joining it with an existing pane. The source pane is removed from its original window and added to the target window. Optionally specify direction (horizontal/vertical) and size. Use this to consolidate panes across windows or reorganize your workspace layout.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "source_pane_id": { "type": "string", "description": "Pane ID to move (e.g., %1)" },
                        "target_pane_id": { "type": "string", "description": "Pane ID to join with in the target window (e.g., %0)" },
                        "direction": { "type": "string", "description": "Split direction: horizontal or vertical (optional)" },
                        "size": { "type": "integer", "description": "Pane size in cells (optional)" }
                    },
                    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
                }
            },
            {
                "name": "swap_pane",
                "description": "Swap the positions of two panes within a session. Both panes retain their state and running processes, but their positions are exchanged. Use this to reorganize pane layout without losing work. Both panes must be in the same session (can be in different windows).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "source_pane_id": { "type": "string", "description": "First pane ID (e.g., %0)" },
                        "target_pane_id": { "type": "string", "description": "Second pane ID to swap with (e.g., %1)" },
                        "detached": { "type": "boolean", "description": "Detach source pane after swap (default: false)" }
                    },
                    "required": ["host", "session_name", "source_pane_id", "target_pane_id"]
                }
            },
            {
                "name": "host_capabilities",
                "description": "Query which features the host's rmux daemon supports. Returns a list of capabilities like 'web.share', 'sdk.waits', 'stream.control'. Use this before attempting advanced operations to verify the host supports the required features. Optionally check for a specific capability — returns ok=true if supported, ok=false otherwise. Useful for feature detection in multi-host environments with varying rmux versions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "check": { "type": "string", "description": "Specific capability to check for (e.g., 'stream.control'). Returns ok=true if supported." }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "capture_region",
                "description": "Capture a rectangular region of a pane's visible content. Specify the region with row, col, rows, and cols parameters (all 0-based). If coordinates are omitted, captures the entire pane (like a screenshot). Supports plain text output (default) or styled output with color markup. Use this to extract specific portions of the screen (e.g., a table, a status bar, a specific UI element).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "row": { "type": "integer", "description": "Top row of region (0-based). Omit all coords for full pane capture." },
                        "col": { "type": "integer", "description": "Left column of region (0-based)" },
                        "rows": { "type": "integer", "description": "Height of region in rows" },
                        "cols": { "type": "integer", "description": "Width of region in columns" },
                        "styled": { "type": "boolean", "description": "Preserve style/color markup (default: false, plain text only)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_for_bytes",
                "description": "Wait for specific raw bytes to appear in the pane output stream. Unlike wait_for_text (which only matches visible text after ANSI processing), this matches the raw byte stream including ANSI escape sequences and control characters. Bytes must be provided as a base64-encoded string. Use this when you need to detect specific terminal sequences (e.g., cursor movements, color changes) that are not visible in text. Default timeout is 30 seconds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "bytes": { "type": "string", "description": "Raw bytes to wait for, encoded as base64" },
                        "only_new": { "type": "boolean", "description": "Only match data appearing after this call (skip existing buffer, default: false)" },
                        "timeout_ms": { "type": "number", "description": "Maximum wait time in milliseconds (default: 30000)" }
                    },
                    "required": ["host", "session_name", "pane_id", "bytes"]
                }
            },
            {
                "name": "wait_stable",
                "description": "Wait until the pane output has been stable (no changes) for a specified duration. Monitors the pane content and returns when it hasn't changed for stable_ms milliseconds. Use this after sending commands to ensure terminal rendering is complete before capturing output. Ideal for commands with progressive output (e.g., builds, downloads) where you want to wait for completion without knowing the exact completion text. Default stable duration is 500ms, default timeout is 30 seconds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "Hostname, e.g. tf01" },
                        "session_name": { "type": "string", "description": "Session name, e.g. agent-ops" },
                        "pane_id": { "type": "string", "description": "Pane ID, e.g. %0" },
                        "stable_ms": { "type": "number", "description": "Duration of stability required in milliseconds (default: 500)" },
                        "timeout_ms": { "type": "number", "description": "Maximum total wait time in milliseconds (default: 30000)" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "deploy_bridge",
                "description": "Deploy a compiled rmux-bridge binary to multiple remote hosts and restart the service. This is for UPGRADE deployments only — target hosts MUST already have rmux-bridge running (first-time deployments must use deploy/install-bridge.sh via SSH). The deployment process for each host: upload binary → replace existing binary → restart service → reconnect to verify. Supports concurrent deployments with configurable concurrency limit. Returns per-host deployment status. Use this to roll out bridge updates across your infrastructure.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hosts": { "type": "array", "items": { "type": "string" }, "description": "Target hostnames (must already have rmux-bridge running)" },
                        "binary_path": { "type": "string", "description": "Local path to compiled rmux-bridge binary (e.g., './target/release/rmux-bridge')" },
                        "remote_path": { "type": "string", "description": "Remote binary path (auto-detected from systemd ExecStart if omitted)" },
                        "concurrency": { "type": "integer", "description": "Max concurrent deployments (default: 3, 0=unlimited)" }
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
