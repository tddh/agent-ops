use super::ProtocolProxy;
use rmux_sdk::SessionName;
use serde_json::json;

impl ProtocolProxy {
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
}
