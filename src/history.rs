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
            .ok_or("Cleanup record is missing run_id; history cannot be fully interpreted")?;
        if positions.contains_key(run_id) {
            continue;
        }
        let item = if record.event == "cleanup_finished" {
            let result: CleanupResult = serde_json::from_value(record.data.clone())
                .map_err(|e| format!("Invalid cleanup result record: {e}"))?;
            if result.schema_version != 1 {
                return Err("Unsupported cleanup result schema_version".into());
            }
            let message = if result.cancelled {
                "Cancelled by the user; sent requests cannot be recalled"
            } else if !result.errors.is_empty() {
                "Cleanup finished with gaps in required records or resource observations"
            } else {
                "Cleanup finished; exit status and system resource observations are recorded separately"
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
                message: "No finish record; cleanup may still be running or was interrupted. Request and exit outcomes are unknown; actions are not replayed"
                    .into(),
            }
        };
        positions.insert(run_id.to_string(), items.len());
        items.push(item);
    }
    items.truncate(limit);
    Ok(items)
}
