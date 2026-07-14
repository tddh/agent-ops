use super::ProtocolProxy;
use super::BUFFER_PARSE_RE;
use serde_json::json;

impl ProtocolProxy {
    pub async fn handle_list_buffers(&self) -> serde_json::Value {
        match self.rmux.cmd(&["list-buffers"]).await {
            Ok(result) => {
                if result.exit != Some(0) {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
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
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'paste-buffer' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_delete_buffer(&self, buffer_name: &str) -> serde_json::Value {
        match self.rmux.cmd(&["delete-buffer", "-b", buffer_name]).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true})
                } else {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'delete-buffer' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }
}
