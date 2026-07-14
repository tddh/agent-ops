use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use super::ToolContext;
use crate::transport::{recv_json_frame, send_json_frame, BridgeStream};

/// 内部 session_create（不记 audit）
pub(crate) async fn create_session_inner(
    stream: &mut BridgeStream,
    session_name: &str,
) -> Result<Value> {
    send_json_frame(
        stream,
        &json!({ "type": "new_session", "name": session_name, "detached": true }),
    )
    .await?;
    recv_json_frame(stream).await
}

/// 解析主机名列表 → (name, Option<HostConfig>)
pub(crate) fn resolve_hosts(
    ctx: &ToolContext,
    names: &[String],
) -> Vec<(String, Option<agent_ops_core::types::HostConfig>)> {
    names
        .iter()
        .map(|name| {
            let h = ctx.router.get(name);
            (name.clone(), h)
        })
        .collect()
}

/// 创建并发信号量（concurrency=0 → None，即不限制）
pub(crate) fn make_semaphore(limit: usize) -> Option<Arc<tokio::sync::Semaphore>> {
    if limit > 0 {
        Some(Arc::new(tokio::sync::Semaphore::new(limit)))
    } else {
        None
    }
}

/// 收集 JoinHandle 结果 → (results_map, success_count, failed_count)
pub(crate) async fn collect_batch_results(
    handles: Vec<tokio::task::JoinHandle<(String, Value)>>,
) -> (serde_json::Map<String, Value>, u32, u32) {
    let mut results_map = serde_json::Map::new();
    let mut success = 0u32;
    let mut failed = 0u32;
    for handle in handles {
        if let Ok((host_name, result)) = handle.await {
            if result["ok"].as_bool().unwrap_or(false) {
                success += 1;
            } else {
                failed += 1;
            }
            results_map.insert(host_name, result);
        } else {
            failed += 1;
            results_map.insert(
                "unknown".into(),
                json!({"ok": false, "error": "task cancelled"}),
            );
        }
    }
    (results_map, success, failed)
}
