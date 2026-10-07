//! Read-only summaries. Missing final records are Unknown, never replay instructions.
use crate::{cleanup::CleanupResult, storage::Store};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub run_id: String,
    pub timestamp_unix_ms: u64,
    pub status: String,
    pub result: Option<CleanupResult>,
    pub message: String,
}
pub fn load(store: &Store, limit: usize) -> Result<Vec<HistoryItem>, String> {
    let records = store.records(usize::MAX)?;
    let mut items = Vec::<HistoryItem>::new();
    let mut positions = HashMap::new();
    for record in records {
        if ![
            "cleanup_started",
            "target_request_prepared",
            "target_request_sent",
            "target_request_outcome",
            "cleanup_finished",
        ]
        .contains(&record.event.as_str())
        {
            continue;
        }
        let run_id = record
            .data
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .ok_or("处理记录缺少 run_id；历史不可完整解读")?;
        if positions.contains_key(run_id) {
            continue;
        }
        let item = if record.event == "cleanup_finished" {
            let result: CleanupResult = serde_json::from_value(record.data.clone())
                .map_err(|e| format!("处理结果记录无效：{e}"))?;
            if result.schema_version != 1 {
                return Err("处理结果 schema_version 不受支持".into());
            }
            let message = if result.cancelled {
                "用户取消；已发请求不能撤销"
            } else if !result.errors.is_empty() {
                "处理结束，必要记录或资源观察有缺口"
            } else {
                "处理结束；退出状态与系统资源观察分别记录"
            };
            HistoryItem {
                run_id: run_id.into(),
                timestamp_unix_ms: record.timestamp_unix_ms,
                status: if result.cancelled {
                    "cancelled"
                } else {
                    "finished"
                }
                .into(),
                result: Some(result),
                message: message.into(),
            }
        } else {
            HistoryItem {
                run_id: run_id.into(),
                timestamp_unix_ms: record.timestamp_unix_ms,
                status: "unfinished".into(),
                result: None,
                message: "没有结束记录，可能仍在进行或被中断；请求／退出结果未知，不重放动作"
                    .into(),
            }
        };
        positions.insert(run_id.to_string(), items.len());
        items.push(item);
    }
    items.truncate(limit);
    Ok(items)
}
