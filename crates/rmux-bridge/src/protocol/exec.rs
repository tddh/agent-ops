use super::ProtocolProxy;
use rmux_sdk::{PaneRespawnOptions, ProcessCommandSpec, ProcessSpec, SessionName};
use serde_json::json;
use std::path::PathBuf;

impl ProtocolProxy {
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
}
