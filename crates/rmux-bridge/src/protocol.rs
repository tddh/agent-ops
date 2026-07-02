//! RMUX protocol layer: connects to a local RMUX daemon over a Unix socket
//! and translates JSON requests into RMUX SDK calls. Handles all 35 tool
//! message types plus pane output streaming.

use anyhow::Result;
use regex::Regex;
use rmux_sdk::{
    EnsureSession, EnsureSessionPolicy, PaneId, PaneOutputChunk, ProcessSpec, SessionName,
    SplitDirection, TerminalSizeSpec,
};
use serde_json::json;
use std::sync::OnceLock;
use tokio::sync::mpsc;

static ANSI_RE: OnceLock<Regex> = OnceLock::new();

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

    pub async fn handle_capture_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        max_lines: Option<usize>,
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

    pub async fn handle_respawn_pane(
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
            Ok(pane) => match pane.respawn(rmux_sdk::PaneRespawnOptions::default()).await {
                Ok(_target) => json!({"ok": true, "respawned": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
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
