//! RMUX protocol layer: connects to a local RMUX daemon over a Unix socket
//! and translates JSON requests into RMUX SDK calls. Handles all 35 tool
//! message types plus pane output streaming.

use anyhow::Result;
use regex::Regex;
use base64::{Engine as _, engine::general_purpose};
use rmux_sdk::{
    capture::{CapturedRegion, Rect},
    EnsureSession, EnsureSessionPolicy, PaneId, PaneOutputChunk, PaneOutputStart,
    PaneProcessState, PaneRespawnOptions, ProcessCommandSpec, ProcessSpec, SessionName,
    SplitDirection, TerminalSizeSpec,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::mpsc;

static ANSI_RE: OnceLock<Regex> = OnceLock::new();
static PANE_ID_RE: OnceLock<Regex> = OnceLock::new();
static BUFFER_PARSE_RE: OnceLock<Regex> = OnceLock::new();

pub struct PaneOutputStream {
    pub rx: mpsc::Receiver<String>,
}

/// Wraps an RMUX SDK connection and exposes JSON-handling methods for each
/// protocol message type used by the MCP server.
pub struct ProtocolProxy {
    rmux: rmux_sdk::Rmux,
}

impl ProtocolProxy {
    /// Connects to the RMUX daemon at the given Unix socket path.
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let rmux = rmux_sdk::Rmux::builder()
            .unix_socket(socket_path)
            .connect()
            .await?;
        Ok(Self { rmux })
    }

    fn parse_pane_id(raw: &str) -> Option<PaneId> {
        raw.strip_prefix('%')
            .and_then(|n| n.parse::<u32>().ok())
            .map(PaneId::new)
    }

    fn clean_text(raw: &str, command: Option<&str>) -> String {
        let ansi_re = ANSI_RE.get_or_init(|| {
            Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").expect("invalid ANSI regex")
        });
        let no_ansi = ansi_re.replace_all(raw, "");

        let mut lines: Vec<&str> = no_ansi.lines().collect();

        if let Some(cmd) = command {
            while let Some(first) = lines.first() {
                let t = first.trim();
                if t.is_empty() || t.contains(cmd) || t == cmd {
                    lines.remove(0);
                } else {
                    break;
                }
            }
        }

        lines
            .into_iter()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty()
                    && !(t.starts_with("root@") && (t.contains('#') || t.contains('$')))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn handle_new_session(&self, name: &str, detached: bool) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        let ensure = EnsureSession::named(session_name.clone())
            .policy(EnsureSessionPolicy::CreateOrReuse)
            .detached(detached)
            .process(ProcessSpec::default());

        match self.rmux.ensure_session(ensure).await {
            Ok(session) => {
                let pane = session.pane(0, 0);
                match pane.id().await {
                    Ok(Some(pane_id)) => {
                        json!({
                            "ok": true,
                            "session_name": name,
                            "pane_id": pane_id.to_string(),
                            "window_count": 1,
                        })
                    }
                    Ok(None) => json!({"ok": false, "error": "pane has no id"}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_list_sessions(&self) -> serde_json::Value {
        match self.rmux.list_sessions().await {
            Ok(sessions) => {
                let list: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| json!({"session_name": s.to_string()}))
                    .collect();
                json!({"ok": true, "sessions": list})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_attach_session(&self, name: &str) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.has_session(session_name).await {
            Ok(true) => json!({"ok": true, "session_name": name, "attached": true}),
            Ok(false) => json!({"ok": false, "error": format!("session not found: {}", name)}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_detach_session(&self, name: &str) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.has_session(session_name).await {
            Ok(true) => json!({"ok": true, "session_name": name, "detached": true}),
            Ok(false) => json!({"ok": false, "error": format!("session not found: {}", name)}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_send_keys(
        &self,
        session_name: &str,
        pane_id_str: &str,
        keys: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.send_key(keys).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_capture_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        max_lines: Option<usize>,
        ansi: Option<bool>,
        start_line: Option<i64>,
        end_line: Option<i64>,
        join_wrapped: Option<bool>,
        preserve_spaces: Option<bool>,
        alternate: Option<bool>,
        buffer_name: Option<String>,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        // Path B: any advanced parameter specified → use PaneCaptureBuilder
        let use_path_b = ansi.is_some()
            || alternate.is_some()
            || start_line.is_some()
            || end_line.is_some()
            || buffer_name.is_some()
            || join_wrapped.is_some()
            || preserve_spaces.is_some();

        if use_path_b {
            let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(p) => p,
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            let mut builder = pane.capture_pane();
            if let Some(ansi) = ansi {
                builder = builder.escape_ansi(ansi);
            }
            if let Some(sl) = start_line {
                builder = builder.start(sl);
            } else if let Some(ml) = max_lines.filter(|&n| n > 0) {
                // max_lines in Path B maps to start_line = -(max_lines) if start_line not set
                builder = builder.start(-(ml as i64));
            }
            if let Some(el) = end_line {
                builder = builder.end(el);
            }
            if let Some(jw) = join_wrapped {
                builder = builder.join_wrapped(jw);
            }
            if let Some(ps) = preserve_spaces {
                builder = builder.preserve_trailing_spaces(ps);
            }
            if let Some(alt) = alternate {
                builder = builder.alternate(alt);
            }
            if let Some(buf_name) = buffer_name {
                builder = builder.buffer(buf_name);
            }
            match builder.await {
                Ok(capture) => {
                    let text = if ansi.unwrap_or(false) {
                        // ANSI output: base64 encode
                        general_purpose::STANDARD.encode(&capture.stdout)
                    } else {
                        String::from_utf8_lossy(&capture.stdout).to_string()
                    };
                    let mut resp = json!({
                        "ok": true,
                        "text": text,
                        "buffer_written": capture.buffer_name,
                    });
                    if ansi.unwrap_or(false) {
                        resp["encoding"] = json!("base64");
                    } else {
                        resp["encoding"] = json!(null);
                    }
                    resp
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        } else {
            // Path A: existing behavior (snapshot + clean_text + max_lines truncation)
            match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(pane) => match pane.snapshot().await {
                    Ok(snapshot) => {
                        let full_text = Self::clean_text(&snapshot.visible_text(), None);
                        let text = if max_lines.is_some_and(|n| n > 0) {
                            let n = max_lines.unwrap();
                            let lines: Vec<&str> = full_text.lines().collect();
                            if lines.len() > n {
                                lines[lines.len() - n..].join("\n")
                            } else {
                                full_text
                            }
                        } else {
                            full_text
                        };
                        json!({"ok": true, "text": text})
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                },
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
    }

    pub async fn handle_wait_for_text(
        &self,
        session_name: &str,
        pane_id_str: &str,
        text: &str,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let timeout = std::time::Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout, pane.wait_for_text(text)).await {
            Ok(Ok(())) => json!({"ok": true, "found": true}),
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(_) => {
                json!({"ok": false, "found": false, "error": format!("timeout waiting for: {}", text)})
            }
        }
    }

    pub async fn handle_wait_exit(
        &self,
        session_name: &str,
        pane_id_str: &str,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let timeout = std::time::Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout, pane.wait_exit()).await {
            Ok(Ok(Some(state))) => {
                json!({"ok": true, "exited": true, "exit_code": state.code, "signal": state.signal})
            }
            Ok(Ok(None)) => json!({"ok": true, "exited": false}),
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(_) => json!({"ok": false, "error": "timeout waiting for exit"}),
        }
    }

    pub async fn handle_split_window(
        &self,
        session_name_str: &str,
        _dir: &str,
    ) -> serde_json::Value {
        let session_name = match SessionName::new(session_name_str) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(session_name).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match session.new_window().await {
            Ok(_) => json!({"ok": true, "window_created": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_spawn_command(
        &self,
        session_name: &str,
        pane_id_str: &str,
        command: &str,
        args: &[String],
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let full_cmd: Vec<String> = std::iter::once(command.to_string())
            .chain(args.iter().cloned())
            .collect();
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.spawn(full_cmd).await {
                Ok(_target) => json!({"ok": true, "spawned": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_shell_command(
        &self,
        session_name: &str,
        pane_id_str: &str,
        cmd: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.shell(cmd).await {
                Ok(_target) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_respawn_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        command: Option<String>,
        args: Option<Vec<String>>,
        shell: Option<bool>,
        cwd: Option<String>,
        env: Option<serde_json::Value>,
        kill: Option<bool>,
        keep_alive_on_exit: Option<bool>,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => {
                let mut opts = PaneRespawnOptions::default();

                if kill == Some(true) {
                    opts.kill = true;
                }

                if let Some(ref cwd_path) = cwd {
                    opts.start_directory = Some(PathBuf::from(cwd_path));
                }

                opts.keep_alive_on_exit = keep_alive_on_exit;

                if let Some(ref cmd) = command {
                    let env_strings: Option<Vec<String>> = env.as_ref().and_then(|env_map| {
                        env_map.as_object().map(|obj| {
                            obj.iter()
                                .map(|(k, v)| {
                                    let val = v.as_str().unwrap_or("");
                                    format!("{}={}", k, val)
                                })
                                .collect()
                        })
                    });
                    let env_not_empty = !matches!(env_strings.as_deref(), Some([]));
                    opts.process = if shell.unwrap_or(false) {
                        ProcessSpec {
                            process_command: Some(ProcessCommandSpec::Shell(cmd.clone())),
                            environment: env_not_empty.then_some(env_strings).flatten(),
                            ..Default::default()
                        }
                    } else {
                        let argv = std::iter::once(cmd.clone())
                            .chain(args.unwrap_or_default())
                            .collect();
                        ProcessSpec {
                            process_command: Some(ProcessCommandSpec::Argv(argv)),
                            environment: env_not_empty.then_some(env_strings).flatten(),
                            ..Default::default()
                        }
                    };
                }

                match pane.respawn(opts).await {
                    Ok(_target) => json!({"ok": true, "respawned": true}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(dead_code)]
    pub fn handle_file_download_open(
        remote_path: &str,
    ) -> std::result::Result<std::fs::File, serde_json::Value> {
        std::fs::File::open(remote_path)
            .map_err(|e| json!({"ok": false, "error": format!("failed to open file: {}", e)}))
    }

    pub async fn handle_broadcast_keys(
        &self,
        session_name: &str,
        pane_ids: &[String],
        keys: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let mut panes = Vec::new();
        for pid_str in pane_ids {
            let pane_id = match Self::parse_pane_id(pid_str) {
                Some(id) => id,
                None => {
                    return json!({"ok": false, "error": format!("invalid pane_id: {}", pid_str)})
                }
            };
            match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(p) => panes.push(p),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        match self
            .rmux
            .broadcast(&panes, rmux_sdk::Input::key(keys))
            .await
        {
            Ok(_result) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_cmd_escape(&self, args: &[String]) -> serde_json::Value {
        match self.rmux.cmd(args).await {
            Ok(result) => json!({
                "ok": true,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit,
            }),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn subscribe_pane_output(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> Result<PaneOutputStream> {
        let pane_id = Self::parse_pane_id(pane_id_str)
            .ok_or_else(|| anyhow::anyhow!("invalid pane_id: {}", pane_id_str))?;
        let sn = SessionName::new(session_name).map_err(|e| anyhow::anyhow!("{}", e))?;
        let pane = self.rmux.get_pane_by_id(&sn, pane_id).await?;
        let snapshot = pane.snapshot().await?;
        let (tx, rx) = mpsc::channel(256);
        let _ = tx.send(snapshot.visible_text()).await;
        let mut stream = pane.output_stream().await?;
        tokio::spawn(async move {
            while let Ok(Some(chunk)) = stream.next().await {
                let text = match chunk {
                    PaneOutputChunk::Bytes { bytes, .. } => {
                        String::from_utf8_lossy(&bytes).to_string()
                    }
                    PaneOutputChunk::Lag(_) => continue,
                    _ => continue,
                };
                if tx.send(text).await.is_err() {
                    break;
                }
            }
        });
        Ok(PaneOutputStream { rx })
    }

    pub async fn handle_split_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        direction: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let dir = match direction {
            "horizontal" | "h" => SplitDirection::Down,
            "vertical" | "v" => SplitDirection::Right,
            _ => {
                return json!({"ok": false, "error": format!("invalid direction: {}. Use horizontal or vertical", direction)})
            }
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.split(dir).await {
                Ok(new_pane) => match new_pane.id().await {
                    Ok(Some(id)) => json!({"ok": true, "pane_id": format!("{}", id)}),
                    Ok(None) => json!({"ok": false, "error": "split pane has no id"}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                },
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_resize_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        cols: u16,
        rows: u16,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.resize(TerminalSizeSpec::new(cols, rows)).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_send_text(
        &self,
        session_name: &str,
        pane_id_str: &str,
        text: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.send_text(text).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_set_pane_title(
        &self,
        session_name: &str,
        pane_id_str: &str,
        title: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.set_title(title).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_pane_text(
        &self,
        session_name: &str,
        pane_id_str: &str,
        pattern: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.find_text(pattern).await {
                Ok(Some(m)) => json!({
                    "ok": true,
                    "found": true,
                    "match": {
                        "start_row": m.start_row, "start_col": m.start_col,
                        "end_row": m.end_row, "end_col": m.end_col,
                        "text": m.text,
                    }
                }),
                Ok(None) => json!({"ok": true, "found": false}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_close_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.close().await {
                Ok(outcome) => {
                    let closed = matches!(outcome, rmux_sdk::PaneCloseOutcome::Closed { .. });
                    json!({"ok": true, "closed": closed})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_close_window(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.close().await {
            Ok(_outcome) => json!({"ok": true, "closed": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_kill_session(&self, session_name: &str) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match session.kill().await {
            Ok(killed) => json!({"ok": true, "killed": killed}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_rename_window(
        &self,
        session_name: &str,
        window_index: u32,
        name: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.rename(name).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_list_window_panes(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.panes().await {
            Ok(panes) => {
                let list: Vec<serde_json::Value> = panes
                    .iter()
                    .map(|wp| {
                        json!({
                            "pane_id": format!("{}", wp.id),
                            "active": wp.active,
                        })
                    })
                    .collect();
                json!({"ok": true, "panes": list})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_resize_window(
        &self,
        session_name: &str,
        window_index: u32,
        width: Option<u16>,
        height: Option<u16>,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.resize(width, height).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_select_window(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.select().await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_select_layout(
        &self,
        session_name: &str,
        window_index: u32,
        layout: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        let layout_name = match layout {
            "even-horizontal" => rmux_sdk::LayoutName::EvenHorizontal,
            "even-vertical" => rmux_sdk::LayoutName::EvenVertical,
            "main-horizontal" => rmux_sdk::LayoutName::MainHorizontal,
            "main-vertical" => rmux_sdk::LayoutName::MainVertical,
            "tiled" => rmux_sdk::LayoutName::Tiled,
            _ => {
                return json!({"ok": false, "error": format!("unknown layout: {}. Use: even-horizontal, even-vertical, main-horizontal, main-vertical, tiled", layout)})
            }
        };
        match window.select_layout(layout_name).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_pane_info(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.info().await {
                Ok(info) => {
                    let pane_info = info.panes.iter().find(|p| p.id == pane_id);
                    match pane_info {
                        Some(p) => json!({
                            "ok": true,
                            "info": {
                                "pane_id": format!("{}", p.id),
                                "window_id": format!("{}", p.window_id),
                                "session_id": format!("{}", p.session_id),
                                "index": p.index,
                                "size_cols": p.size.cols,
                                "size_rows": p.size.rows,
                                "command": p.command,
                                "working_directory": p.working_directory,
                                "tags": p.tags,
                            }
                        }),
                        None => json!({"ok": false, "error": "pane not found in info snapshot"}),
                    }
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_window_info(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.info().await {
            Ok(info) => {
                let win_info = info.windows.iter().find(|w| w.index == window_index);
                match win_info {
                    Some(w) => json!({
                        "ok": true,
                        "info": {
                            "window_id": format!("{}", w.id),
                            "size_cols": w.size.cols,
                            "size_rows": w.size.rows,
                            "name": w.name,
                            "index": w.index,
                        }
                    }),
                    None => json!({"ok": false, "error": "window not found in info snapshot"}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_pane_exists(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.exists().await {
                Ok(exists) => json!({"ok": true, "exists": exists}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_panes(&self, args: &serde_json::Value) -> serde_json::Value {
        let mut finder = self.rmux.find_panes();

        if let Some(s) = args["session_name"].as_str() {
            finder = finder.session(s);
        }
        if let Some(t) = args["title"].as_str() {
            finder = finder.title(t);
        }
        if let Some(p) = args["title_prefix"].as_str() {
            finder = finder.title_prefix(p);
        }
        if let Some(c) = args["command_contains"].as_str() {
            finder = finder.command_contains(c);
        }
        if let Some(d) = args["cwd_contains"].as_str() {
            finder = finder.cwd_contains(d);
        }
        if let Some(idx) = args["window_index"].as_u64() {
            finder = finder.window_index(idx as u32);
        }
        let running = args["running"].as_bool().unwrap_or(false);
        let exited = args["exited"].as_bool().unwrap_or(false);

        if running && exited {
            return json!({"ok": false, "error": "running and exited are mutually exclusive"});
        }
        if running {
            finder = finder.running();
        }
        if exited {
            finder = finder.exited();
        }

        match finder.all().await {
            Ok(panes) => {
                let list: Vec<serde_json::Value> = panes
                    .iter()
                    .map(|d| {
                        let (process_str, pid): (String, Option<u32>) = match &d.process {
                            PaneProcessState::Unknown => ("unknown".to_string(), None),
                            PaneProcessState::Running { pid } => {
                                ("running".to_string(), *pid)
                            }
                            PaneProcessState::Exited => ("exited".to_string(), None),
                            _ => ("unknown".to_string(), None),
                        };
                        let mut obj = json!({
                            "pane_id": format!("{}", d.pane_id),
                            "session_name": d.session_name.to_string(),
                            "session_id": format!("{}", d.session_id),
                            "window_id": format!("{}", d.window_id),
                            "window_index": d.window_index,
                            "pane_index": d.pane_index,
                            "title": d.title,
                            "command": d.command,
                            "working_directory": d.working_directory,
                            "process": process_str,
                            "tags": d.tags,
                        });
                        if let Some(p) = pid {
                            obj["pid"] = json!(p);
                        }
                        obj
                    })
                    .collect();
                json!({"ok": true, "panes": list, "count": list.len()})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_sessions(&self, args: &serde_json::Value) -> serde_json::Value {
        let mut finder = self.rmux.find_sessions();

        if let Some(name) = args["name"].as_str() {
            finder = finder.name(name);
        }

        match finder.all().await {
            Ok(sessions) => {
                let list: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|d| {
                        json!({"session_name": d.name.to_string()})
                    })
                    .collect();
                json!({"ok": true, "sessions": list, "count": list.len()})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_get_pane_title(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.title().await {
                Ok(title) => {
                    json!({"ok": true, "pane_id": pane_id_str, "title": title})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_text_all(
        &self,
        session_name: &str,
        pane_id_str: &str,
        pattern: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.find_text_all(pattern).await {
                Ok(matches) => {
                    let list: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            json!({
                                "start_row": m.start_row,
                                "start_col": m.start_col,
                                "end_row": m.end_row,
                                "end_col": m.end_col,
                                "text": m.text,
                            })
                        })
                        .collect();
                    json!({"ok": true, "matches": list, "count": list.len()})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    // === Phase 2+3 handlers ===

    /// 5. clear_history (CLI)
    pub async fn handle_clear_history(
        &self,
        _session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        match self.rmux.cmd(&["clear-history", "-t", &pane_id.to_string()]).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'clear-history' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 6a. list_buffers (CLI)
    pub async fn handle_list_buffers(&self) -> serde_json::Value {
        match self.rmux.cmd(&["list-buffers"]).await {
            Ok(result) => {
                if result.exit != Some(0) {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    return json!({"ok": false, "error": format!("CLI command 'list-buffers' exited with code {}: {}", code, stderr)});
                }
                let stdout = String::from_utf8_lossy(&result.stdout).to_string();
                let re = BUFFER_PARSE_RE.get_or_init(|| {
                    regex::Regex::new(r#"^(\S+):\s+(\d+)\s+bytes:\s+"?(.*?)"?$"#).unwrap()
                });
                let buffers: Vec<serde_json::Value> = stdout
                    .lines()
                    .filter_map(|line| {
                        let caps = re.captures(line)?;
                        let name = caps.get(1)?.as_str().to_string();
                        let size: u64 = caps.get(2)?.as_str().parse().ok()?;
                        let preview = caps.get(3)?.as_str().to_string();
                        Some(json!({"name": name, "size": size, "preview": preview}))
                    })
                    .collect();
                let count = buffers.len();
                json!({"ok": true, "buffers": buffers, "count": count})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 6b. paste_buffer (CLI)
    pub async fn handle_paste_buffer(
        &self,
        _session_name: &str,
        pane_id_str: &str,
        buffer_name: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let mut args = vec!["paste-buffer"];
        if !buffer_name.is_empty() {
            args.push("-b");
            args.push(buffer_name);
        }
        args.push("-t");
        let pane_str = pane_id.to_string();
        args.push(&pane_str);
        match self.rmux.cmd(&args).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'paste-buffer' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 6c. delete_buffer (CLI)
    pub async fn handle_delete_buffer(&self, buffer_name: &str) -> serde_json::Value {
        match self.rmux.cmd(&["delete-buffer", "-b", buffer_name]).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'delete-buffer' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 7. split_pane_with (SDK: PaneSplitBuilder)
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_split_pane_with(
        &self,
        session_name: &str,
        pane_id_str: &str,
        direction: &str,
        command: &str,
        args: &[String],
        shell: bool,
        cwd: Option<String>,
        env: Option<serde_json::Value>,
        title: Option<String>,
        keep_alive_on_exit: Option<bool>,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let dir = match direction {
            "horizontal" | "h" => SplitDirection::Down,
            "vertical" | "v" => SplitDirection::Right,
            _ => {
                return json!({"ok": false, "error": format!("invalid direction: {}. Use horizontal or vertical", direction)})
            }
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => {
                let builder = pane.split_with(dir);
                let mut cmd_builder = if shell {
                    builder.shell(command)
                } else {
                    let argv: Vec<String> = std::iter::once(command.to_string())
                        .chain(args.iter().cloned())
                        .collect();
                    builder.spawn(argv)
                };
                if let Some(ref cwd_path) = cwd {
                    cmd_builder = cmd_builder.cwd(PathBuf::from(cwd_path));
                }
                if let Some(ref env_map) = env {
                    if let Some(obj) = env_map.as_object() {
                        for (k, v) in obj {
                            if let Some(val_str) = v.as_str() {
                                cmd_builder = cmd_builder.env(k, val_str);
                            }
                        }
                    }
                }
                if let Some(ref t) = title {
                    cmd_builder = cmd_builder.title(t);
                }
                if let Some(keep) = keep_alive_on_exit {
                    cmd_builder = cmd_builder.keep_alive_on_exit(keep);
                }
                match cmd_builder.await {
                    Ok(new_pane) => match new_pane.id().await {
                        Ok(Some(id)) => json!({"ok": true, "new_pane_id": format!("{}", id)}),
                        Ok(None) => json!({"ok": false, "error": "split pane has no id"}),
                    Err(e) => json!({"ok": false, "found": true, "error": e.to_string()}),
                    },
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 8. get_pane_by_title (SDK)
    pub async fn handle_get_pane_by_title(&self, title: &str) -> serde_json::Value {
        match self.rmux.get_pane_by_title(title).await {
            Ok(pane) => {
                let (pane_title, title_error) = match pane.title().await {
                    Ok(t) => (t.unwrap_or_default(), None),
                    Err(e) => (String::new(), Some(e.to_string())),
                };
                let (pane_id, id_error) = match pane.id().await {
                    Ok(id) => (id, None),
                    Err(e) => (None, Some(e.to_string())),
                };
                match pane.info().await {
                    Ok(info) => {
                        let pane_info = info.panes.iter().find(|p| {
                            pane_id.as_ref().is_some_and(|id| p.id == *id)
                        });
                        match pane_info {
                            Some(p) => {
                                let (process_str, pid): (String, Option<u32>) = match &p.process {
                                    PaneProcessState::Unknown => ("unknown".to_string(), None),
                                    PaneProcessState::Running { pid } => {
                                        ("running".to_string(), *pid)
                                    }
                                    PaneProcessState::Exited => ("exited".to_string(), None),
                                    _ => ("unknown".to_string(), None),
                                };
                                let mut obj = json!({
                                    "pane_id": format!("{}", p.id),
                                    "session_id": format!("{}", p.session_id),
                                    "window_id": format!("{}", p.window_id),
                                    "pane_index": p.index,
                                    "title": pane_title,
                                    "command": p.command,
                                    "working_directory": p.working_directory,
                                    "process": process_str,
                                    "tags": p.tags,
                                });
                                if let Some(p) = pid {
                                    obj["pid"] = json!(p);
                                }
                                let mut resp = json!({"ok": true, "found": true, "pane": obj});
                                if let Some(e) = title_error {
                                    resp["title_error"] = json!(e);
                                }
                                if let Some(e) = id_error {
                                    resp["id_error"] = json!(e);
                                }
                                resp
                            }
                            None => {
                                let mut resp = json!({"ok": false, "found": false, "error": format!("no pane found with title: {}", title)});
                                if let Some(e) = title_error {
                                    resp["title_error"] = json!(e);
                                }
                                if let Some(e) = id_error {
                                    resp["id_error"] = json!(e);
                                }
                                resp
                            }
                        }
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "found": false, "error": e.to_string()}),
        }
    }

    /// 9. collect_until_exit (SDK, uses tokio::spawn)
    pub async fn handle_collect_until_exit(
        &self,
        session_name: &str,
        pane_id_str: &str,
        max_bytes: usize,
        timeout_ms: u64,
        starting_at: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let start_kind = if starting_at == "oldest" {
            PaneOutputStart::Oldest
        } else {
            PaneOutputStart::Now
        };
        let handle = tokio::spawn(async move {
            if matches!(start_kind, PaneOutputStart::Oldest) {
                pane.collect_output_until_exit_starting_at(PaneOutputStart::Oldest, max_bytes)
                    .await
            } else {
                pane.collect_output_until_exit(max_bytes).await
            }
        });
        let abort = handle.abort_handle();
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), handle).await {
            Ok(Ok(Ok(output))) => {
                let base64_output = general_purpose::STANDARD.encode(&output.bytes);
                let collected_bytes = output.bytes.len();
                let (exit_code, signal, message) = match &output.exit_state {
                    Some(state) => (state.code, state.signal, state.message.clone()),
                    None => (None, None, None),
                };
                let mut resp = json!({
                    "ok": true,
                    "output": base64_output,
                    "collected_bytes": collected_bytes,
                    "exit_code": exit_code,
                    "signal": signal,
                    "truncated": output.truncated,
                    "lagged": output.lagged,
                    "missed_events": output.missed_events,
                });
                if let Some(ref msg) = message {
                    resp["message"] = json!(msg);
                }
                resp
            }
            Ok(Ok(Err(e))) => json!({"ok": false, "error": e.to_string()}),
            Ok(Err(join_err)) => {
                json!({"ok": false, "error": format!("join error: {}", join_err)})
            }
            Err(_) => {
                abort.abort();
                json!({"ok": false, "error": format!("timeout after {}ms", timeout_ms)})
            }
        }
    }

    /// 10. break_pane (CLI)
    pub async fn handle_break_pane(
        &self,
        _session_name: &str,
        pane_id_str: &str,
        destination_window: Option<u32>,
        detached: bool,
    ) -> serde_json::Value {
        if !pane_id_str.is_empty() && Self::parse_pane_id(pane_id_str).is_none() {
            return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)});
        }
        let win_str = destination_window.map(|dw| format!(":{}", dw));
        let mut args: Vec<String> = vec!["break-pane".to_string()];
        if !pane_id_str.is_empty() {
            args.push("-s".to_string());
            args.push(pane_id_str.to_string());
        }
        if let Some(ref win) = win_str {
            args.push("-t".to_string());
            args.push(win.clone());
        }
        if detached {
            args.push("-d".to_string());
        }
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match self.rmux.cmd(&args_refs).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    let re = PANE_ID_RE.get_or_init(|| regex::Regex::new(r"%(\d+)").unwrap());
                    let stdout = String::from_utf8_lossy(&result.stdout);
                    let pid_opt = re.find(&stdout)
                        .map(|m| format!("%{}", m.as_str().trim_start_matches('%')));
                    json!({"ok": true, "pane_id": pid_opt.unwrap_or_default(), "window_index": destination_window})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'break-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 11. join_pane (CLI)
    pub async fn handle_join_pane(
        &self,
        _session_name: &str,
        source_pane_id: &str,
        target_pane_id: &str,
        direction: Option<&str>,
        size: Option<u32>,
    ) -> serde_json::Value {
        if Self::parse_pane_id(source_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid source_pane_id: {}", source_pane_id)});
        }
        if Self::parse_pane_id(target_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid target_pane_id: {}", target_pane_id)});
        }
        let size_str = size.map(|s| s.to_string());
        let mut args: Vec<String> = vec![
            "join-pane".to_string(),
            "-s".to_string(),
            source_pane_id.to_string(),
            "-t".to_string(),
            target_pane_id.to_string(),
        ];
        if let Some(d) = direction {
            match d {
                "horizontal" | "h" => args.push("-h".to_string()),
                "vertical" | "v" => args.push("-v".to_string()),
                invalid => return json!({"ok": false, "error": format!("invalid direction: {}. Expected 'horizontal'/'h' or 'vertical'/'v'", invalid)}),
            }
        }
        if let Some(ref s) = size_str {
            args.push("-l".to_string());
            args.push(s.clone());
        }
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match self.rmux.cmd(&args_refs).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true, "source_pane_id": source_pane_id, "target_pane_id": target_pane_id})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'join-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 12. swap_pane (CLI)
    pub async fn handle_swap_pane(
        &self,
        _session_name: &str,
        source_pane_id: &str,
        target_pane_id: &str,
        detached: bool,
    ) -> serde_json::Value {
        if Self::parse_pane_id(source_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid source_pane_id: {}", source_pane_id)});
        }
        if Self::parse_pane_id(target_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid target_pane_id: {}", target_pane_id)});
        }
        let mut args = vec!["swap-pane", "-s", source_pane_id, "-t", target_pane_id];
        if detached {
            args.push("-d");
        }
        match self.rmux.cmd(&args).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true, "source_pane_id": source_pane_id, "target_pane_id": target_pane_id})
                } else {
                    let code = result.exit.map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'swap-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 13. capabilities (SDK)
    pub async fn handle_capabilities(&self, check: Option<&str>) -> serde_json::Value {
        match self.rmux.capabilities().await {
            Ok(capabilities) => {
                let mut resp = json!({
                    "ok": true,
                    "capabilities": capabilities,
                    "count": capabilities.len(),
                });
                if let Some(c) = check {
                    match self.rmux.has_capability(c).await {
                        Ok(has) => {
                            resp["has_capability"] = json!(has);
                        }
                        Err(e) => return json!({"ok": false, "error": e.to_string()}),
                    }
                }
                resp
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 14. capture_region (SDK: CaptureBuilder)
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_capture_region(
        &self,
        session_name: &str,
        pane_id_str: &str,
        row: Option<u16>,
        col: Option<u16>,
        rows: Option<u16>,
        cols: Option<u16>,
        styled: bool,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)}),
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        let result = match (row, col, rows, cols) {
            (Some(r), Some(c), Some(rs), Some(cs)) => {
                if rs == 0 || cs == 0 {
                    return json!({"ok": false, "error": "rows and cols must be non-zero"});
                }
                let rect = Rect {
                    row: r,
                    col: c,
                    rows: rs,
                    cols: cs,
                };
                pane.capture_region(rect).preserve_style(styled).await
            }
            (None, None, None, None) => pane.screenshot().preserve_style(styled).await,
            _ => {
                return json!({
                    "ok": false,
                    "error": "all 4 coordinates required (row, col, rows, cols), or omit all for full screenshot"
                });
            }
        };

        match result {
            Ok(CapturedRegion { text, .. }) => {
                json!({"ok": true, "text": text, "styled": styled})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 15. wait_for_bytes (SDK: wait_for / wait_for_next)
    pub async fn handle_wait_for_bytes(
        &self,
        session_name: &str,
        pane_id_str: &str,
        bytes_b64: &str,
        only_new: bool,
        _timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)}),
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let decoded = match general_purpose::STANDARD.decode(bytes_b64) {
            Ok(v) => v,
            Err(e) => return json!({"ok": false, "error": format!("invalid base64: {}", e)}),
        };

        let result = if only_new {
            if self.rmux.has_capability("sdk.waits.armed").await.unwrap_or(false) {
                match pane.wait_for_next(&decoded).await {
                    Ok(armed) => armed.await,
                    Err(e) => Err(e),
                }
            } else {
                pane.wait_for(&decoded).await
            }
        } else {
            pane.wait_for(&decoded).await
        };

        match result {
            Ok(()) => json!({"ok": true, "found": true}),
            Err(e) => json!({"ok": false, "found": false, "error": e.to_string()}),
        }
    }

    /// 16. wait_stable (SDK: wait_until_stable_for)
    pub async fn handle_wait_stable(
        &self,
        session_name: &str,
        pane_id_str: &str,
        stable_ms: u64,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)}),
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        if stable_ms == 0 {
            return json!({"ok": false, "error": "stable_ms must be positive (0 is meaningless for stability check)"});
        }

        match pane
            .wait_until_stable_for(std::time::Duration::from_millis(stable_ms))
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .await
        {
            Ok(_snapshot) => json!({"ok": true, "stable": true}),
            Err(e) => json!({
                "ok": false,
                "stable": false,
                "error": format!("timeout: pane did not stabilize within {}ms: {}", timeout_ms, e)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pane_id_valid() {
        assert!(ProtocolProxy::parse_pane_id("%0").is_some());
        assert!(ProtocolProxy::parse_pane_id("%42").is_some());
        assert!(ProtocolProxy::parse_pane_id("%999").is_some());
    }

    #[test]
    fn test_parse_pane_id_invalid() {
        assert!(ProtocolProxy::parse_pane_id("0").is_none()); // no % prefix
        assert!(ProtocolProxy::parse_pane_id("%abc").is_none()); // non-numeric
        assert!(ProtocolProxy::parse_pane_id("").is_none()); // empty
        assert!(ProtocolProxy::parse_pane_id("%-1").is_none()); // negative
    }

    #[test]
    fn test_clean_text_strips_ansi() {
        let input = "\x1B[31mred text\x1B[0m";
        let cleaned = ProtocolProxy::clean_text(input, None);
        assert!(!cleaned.contains('\x1B'));
        assert!(cleaned.contains("red text"));
    }

    #[test]
    fn test_clean_text_strips_prompt() {
        let input = "root@host:~# ls\nfile1\nfile2";
        let cleaned = ProtocolProxy::clean_text(input, None);
        assert!(!cleaned.contains("root@"));
        assert!(cleaned.contains("file1"));
        assert!(cleaned.contains("file2"));
    }

    #[test]
    fn test_clean_text_strips_command_echo() {
        let input = "ls\nfile1\nfile2";
        let cleaned = ProtocolProxy::clean_text(input, Some("ls"));
        assert!(!cleaned.contains("ls"));
        assert!(cleaned.contains("file1"));
    }

    #[test]
    fn test_clean_text_strips_empty_lines() {
        let input = "\n\nhello\n\n";
        let cleaned = ProtocolProxy::clean_text(input, None);
        assert_eq!(cleaned, "hello");
    }
}
