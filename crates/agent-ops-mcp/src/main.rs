mod audit;
mod files;
mod router;
mod tools;
mod transport;

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
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = PathBuf::from(home).join(".agent-ops");
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
        audit_db,
        agent_name: std::sync::Mutex::new("unknown".to_string()),
    });

    let tools_definition = serde_json::json!({
        "tools": [
            {
                "name": "host_list",
                "description": "列出所有已知的远程主机及其状态",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            },
            {
                "name": "host_filter",
                "description": "按条件过滤主机（group/tags/labels/name pattern）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "group": { "type": "string", "description": "按分组过滤" },
                        "tags": { "type": "array", "items": { "type": "string" }, "description": "同时匹配所有 tag" },
                        "label_key": { "type": "string", "description": "标签键" },
                        "label_value": { "type": "string", "description": "标签值" },
                        "pattern": { "type": "string", "description": "主机名 glob 模式，如 prod-web-*" }
                    }
                }
            },
            {
                "name": "session_create",
                "description": "在指定主机上创建新的终端会话",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "主机名称" },
                        "session_name": { "type": "string", "description": "会话名称（可选）" }
                    },
                    "required": ["host"]
                }
            },
            {
                "name": "session_list",
                "description": "列出指定主机上的所有活动会话",
                "inputSchema": {
                    "type": "object",
                    "properties": { "host": { "type": "string", "description": "主机名称" } },
                    "required": ["host"]
                }
            },
            {
                "name": "session_attach",
                "description": "检查会话是否存在（当前仅检查存在性，不执行真正的 attach/detach）",
                "inputSchema": {
                    "type": "object",
                    "properties": { "host": { "type": "string" }, "session_name": { "type": "string" } },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "session_detach",
                "description": "检查会话是否存在（当前仅检查存在性，不执行真正的 attach/detach）",
                "inputSchema": {
                    "type": "object",
                    "properties": { "host": { "type": "string" }, "session_name": { "type": "string" } },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "send_keys",
                "description": "向终端窗格发送按键。支持特殊键：Ctrl-C=\\x03, Enter=\\n, Tab=\\t, Escape=\\x1b。普通文本请用 send_text",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "keys": { "type": "string", "description": "要发送的按键字符串" }
                    },
                    "required": ["host", "session_name", "pane_id", "keys"]
                }
            },
            {
                "name": "capture_pane",
                "description": "捕获终端窗格的文本内容（默认返回最后 200 行，设 max_lines=0 返回全部 scrollback）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "max_lines": { "type": "integer", "description": "最多返回行数（默认 200，设 0 表示不限制，返回全部 scrollback）" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "wait_for_text",
                "description": "等待终端窗格中出现指定文本（带超时）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "text": { "type": "string" },
                        "timeout_ms": { "type": "number", "description": "超时毫秒数，默认 30000" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "spawn_command",
                "description": "在窗格中启动新进程（直接 exec，替换当前进程）。要求 pane 中无运行进程，否则返回错误。如仅需在运行中的 pane 执行命令，请用 exec 工具",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "command": { "type": "string", "description": "要执行的命令" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "命令参数" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "shell_command",
                "description": "通过 shell 在窗格中执行命令（/bin/sh -c）。要求 pane 中无运行进程，否则返回错误。如仅需在运行中的 pane 执行命令，请用 exec 工具",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "command": { "type": "string", "description": "要执行的 shell 命令" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "respawn_pane",
                "description": "重启窗格进程（默认选项）。用于 pane 中进程已退出或需要重置 shell 环境时",
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
                "name": "wait_exit",
                "description": "等待窗格中进程退出并返回退出状态",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "timeout_ms": { "type": "number", "description": "超时毫秒数，默认 30000" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "split_window",
                "description": "在会话中创建新窗口（空窗口，不含额外 pane）。如需 pane 左右/上下分屏，请用 split_pane。direction 参数当前无效，仅用于兼容",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "direction": { "type": "string", "description": "horizontal 或 vertical" }
                    },
                    "required": ["host", "session_name"]
                }
            },
            {
                "name": "stream_pane",
                "description": "【当前不可用 — MCP 协议不支持服务端推送】订阅窗格输出流。替代方案：用 send_keys 发送命令，然后用 capture_pane 多次轮询获取新输出",
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
                "name": "file_upload",
                "description": "上传文件或目录到远程主机（通过 bridge TLS 通道）。目标目录不存在时自动创建。支持通过 overwrite 控制覆盖策略（overwrite/skip/rename/error），通过 exclude 排除文件（glob 模式）。\n⚠️ 除非用户明确指定，不要自作主张添加 exclude 或修改 overwrite。用户说传什么就传什么，不明白就二次确认。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "local_path": { "type": "string" },
                        "remote_path": { "type": "string" },
                        "overwrite": { "type": "string", "description": "覆盖策略: overwrite(默认,覆盖) | skip(跳过已存在) | rename(重命名) | error(存在则报错)" },
                        "exclude": { "type": "array", "items": { "type": "string" }, "description": "glob 排除模式, 如 [\"*.log\", \"target/**/*\"]。仅用户明确指定时传入" }
                    },
                    "required": ["host", "local_path", "remote_path"]
                }
            },
            {
                "name": "file_download",
                "description": "从远程主机下载文件（通过 bridge TLS 通道）。返回文件大小和 SHA256 校验值。\n⚠️ 除非用户明确指定，不要添加额外过滤或修改路径。用户说下载什么就下载什么，不明白就二次确认。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "remote_path": { "type": "string" },
                        "local_path": { "type": "string" }
                    },
                    "required": ["host", "remote_path", "local_path"]
                }
            },
            {
                "name": "exec",
                "description": "在远程终端执行命令并返回结构化结果（一步完成：发送命令→等待完成→捕获输出→清洗文本，默认返回最后 200 行）。内部用 sentinel 等待命令结束，超时返回失败。适用于一次性会自行退出的命令（ls, cat, grep, systemctl, kubectl, apt-get, curl 等）。不适用于交互式程序（vim, htop, less）和不自动退出的命令（tail -f, nc -l, ping）。对不自动退出的命令，请用 send_keys 发送，然后用 wait_for_text/capture_pane 观察输出，用 send_keys(\"\\x03\") 发送 Ctrl-C 中断。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "主机名称" },
                        "session_name": { "type": "string", "description": "会话名称" },
                        "pane_id": { "type": "string", "description": "窗格 ID，如 %4" },
                        "command": { "type": "string", "description": "要执行的命令" },
                        "timeout_ms": { "type": "number", "description": "超时毫秒数，默认 30000" },
                        "max_lines": { "type": "integer", "description": "最多返回行数（默认 200，设 0 表示不限制）" },
                        "clear_screen": { "type": "boolean", "description": "执行前是否清屏（默认 false）" }
                    },
                    "required": ["host", "session_name", "pane_id", "command"]
                }
            },
            {
                "name": "split_pane",
                "description": "在当前窗格内分屏，返回新 pane 的 ID。horizontal=上下分屏，vertical=左右分屏",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string", "description": "要分割的窗格 ID" },
                        "direction": { "type": "string", "description": "horizontal=上下分屏, vertical=左右分屏" }
                    },
                    "required": ["host", "session_name", "pane_id", "direction"]
                }
            },
            {
                "name": "resize_pane",
                "description": "调整窗格尺寸（列数 x 行数）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "cols": { "type": "integer", "description": "列数（宽度），默认 80" },
                        "rows": { "type": "integer", "description": "行数（高度），默认 24" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "send_text",
                "description": "向终端窗格发送纯文本。与 send_keys 的区别：send_text 适合发普通命令，send_keys 适合发特殊键（Ctrl-C 等）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "text": { "type": "string", "description": "要发送的文本" }
                    },
                    "required": ["host", "session_name", "pane_id", "text"]
                }
            },
            {
                "name": "set_pane_title",
                "description": "设置窗格标题",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "title": { "type": "string", "description": "新标题" }
                    },
                    "required": ["host", "session_name", "pane_id", "title"]
                }
            },
            {
                "name": "find_pane_text",
                "description": "在窗格可见文本中搜索指定内容，返回匹配位置",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" },
                        "pattern": { "type": "string", "description": "搜索模式" }
                    },
                    "required": ["host", "session_name", "pane_id", "pattern"]
                }
            },
            {
                "name": "broadcast_keys",
                "description": "向同一会话中的多个 pane 同时发送相同按键。支持特殊键（同 send_keys）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_ids": { "type": "array", "items": { "type": "string" }, "description": "目标 pane ID 列表" },
                        "keys": { "type": "string", "description": "要发送的按键" }
                    },
                    "required": ["host", "session_name", "pane_ids", "keys"]
                }
            },
            {
                "name": "cmd_escape",
                "description": "直接调用 rmux 命令行工具（如 list-sessions、list-panes）。当 bridge 协议未覆盖某些操作时使用",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" }, "description": "rmux CLI 参数" }
                    },
                    "required": ["host", "args"]
                }
            },
            {
                "name": "close_pane",
                "description": "关闭终端窗格（杀死 pane 进程）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string", "description": "窗格 ID，如 %4" }
                    },
                    "required": ["host", "session_name", "pane_id"]
                }
            },
            {
                "name": "rename_window",
                "description": "重命名窗口",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "name": { "type": "string", "description": "新名称" }
                    },
                    "required": ["host", "session_name", "window_index", "name"]
                }
            },
            {
                "name": "list_window_panes",
                "description": "列出窗口中所有窗格",
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
                "description": "调整窗口尺寸",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "width": { "type": "integer", "description": "宽度（可选）" },
                        "height": { "type": "integer", "description": "高度（可选）" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "select_window",
                "description": "将指定窗口设为活跃窗口",
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
                "description": "应用窗口布局（even-horizontal, even-vertical, main-horizontal, main-vertical, tiled）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer" },
                        "layout": { "type": "string", "description": "布局名称" }
                    },
                    "required": ["host", "session_name", "window_index", "layout"]
                }
            },
            {
                "name": "close_window",
                "description": "关闭终端窗口（杀死 window 及其所有 pane）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "window_index": { "type": "integer", "description": "窗口索引（从 0 开始）" }
                    },
                    "required": ["host", "session_name", "window_index"]
                }
            },
            {
                "name": "kill_session",
                "description": "销毁整个终端会话",
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
                "description": "获取窗格的详细信息（ID、尺寸、命令、工作目录等）",
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
                "description": "获取窗口的详细信息（名称、尺寸、索引）。需列出窗格请用 list_window_panes",
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
                "description": "检查指定窗格是否存在",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "session_name": { "type": "string" },
                        "pane_id": { "type": "string" }
                    },
                    "required": ["host", "session_name", "pane_id"]
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
                *ctx.agent_name.lock().unwrap() = agent_name;
                json_rpc_response(id, &json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agent-ops-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "你是通过 agent-ops 管理远程主机的 AI Agent。\n\n## 核心规则\n1. 如果目标主机名在 agent-ops 配置中（用 host_list 查看），所有操作必须通过 agent-ops 工具执行，禁止手动运行 ssh/scp/rsync 等命令。\n2. 默认会话名: \"agent-ops\"。操作前先检查该会话是否存在（session_attach），如不存在则创建（session_create host=xxx session_name=\"agent-ops\"）。\n3. 文件传输用 file_upload / file_download，不要用 scp。\n4. 命令执行用 exec（自动等待退出）或 send_keys（交互式程序）。\n\n## 工作流\n- 先 host_list 确认目标主机\n- session_attach host=xxx session_name=\"agent-ops\" → 不存在则 session_create\n- 用 exec / send_keys 执行命令，用 capture_pane 查看输出"
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
