use crate::model::{Category, Metric, Pressure, Snapshot, Validity, safe_text};
use std::io::{self, Write};

pub fn bytes(value: u64) -> String {
    if value >= 1 << 30 {
        format!("{:.2} GiB", value as f64 / (1_u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", value as f64 / (1_u64 << 20) as f64)
    }
}

pub fn metric_bytes(metric: &Metric<u64>) -> String {
    match (metric.status, metric.value) {
        (Validity::Ok, Some(value)) => bytes(value),
        (Validity::Denied, _) => "— / 无权限".into(),
        _ => "— / 未知".into(),
    }
}

pub fn pressure_text(metric: &Metric<Pressure>) -> String {
    match (metric.status, metric.value) {
        (Validity::Ok, Some(Pressure::Normal)) => "正常".into(),
        (Validity::Ok, Some(Pressure::Elevated)) => "较高".into(),
        (Validity::Ok, Some(Pressure::High)) => "很高".into(),
        _ => "未知".into(),
    }
}

pub fn category_text(category: Category) -> &'static str {
    match category {
        Category::Application => "应用",
        Category::AiDevelopment => "AI / 开发",
        Category::System => "系统",
        Category::Unknown => "未知归属",
    }
}

pub fn write_json(value: &impl serde::Serialize) -> Result<(), String> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).map_err(|e| e.to_string())?;
    writeln!(handle).map_err(|e| e.to_string())
}

pub fn write_text(value: &str) -> Result<(), String> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{value}").map_err(|e| e.to_string())
}

pub fn status_text(snapshot: &Snapshot) -> String {
    let memory = &snapshot.system;
    format!(
        "bree · 只读 Alpha\n内存压力：{}\n已用 {} / 总量 {}\n压缩 {} · 交换空间 {}\n采样时间：{} Unix ms · 采集 {} ms\n覆盖：枚举 {}，可读内存 {}，可靠身份 {}\n口径：{}\n清理尚未启用。应用组占用合计不等于可释放量。",
        pressure_text(&memory.pressure),
        metric_bytes(&memory.used_bytes),
        metric_bytes(&memory.total_bytes),
        metric_bytes(&memory.compressed_bytes),
        metric_bytes(&memory.swap_used_bytes),
        snapshot.sampled_at_unix_ms,
        snapshot.collected_in_ms,
        snapshot.coverage.enumerated_processes,
        snapshot.coverage.readable_memory_processes,
        snapshot.coverage.reliable_identity_processes,
        safe_text(&memory.used_definition)
    )
}

pub fn list_text(snapshot: &Snapshot, limit: Option<usize>) -> String {
    let mut text = status_text(snapshot);
    text.push_str("\n\n占用\t实例\t分类\t名称\t对象 ID\n");
    let count = limit
        .unwrap_or(snapshot.groups.len())
        .min(snapshot.groups.len());
    for group in snapshot.groups.iter().take(count) {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            metric_bytes(&group.memory_bytes),
            group.process_ids.len(),
            category_text(group.category),
            safe_text(&group.name),
            safe_text(&group.id)
        ));
    }
    if count < snapshot.groups.len() {
        text.push_str(&format!(
            "显示 {count}/{} 组；省略 --limit 查看全部。\n",
            snapshot.groups.len()
        ));
    }
    text
}

/// Normal exports omit executable and app paths. Inspect may reveal a redacted path.
pub fn export_snapshot(snapshot: &Snapshot, include_paths: bool) -> Snapshot {
    let mut result = snapshot.clone();
    let home = std::env::var("HOME").ok();
    for process in &mut result.processes {
        process.name = safe_text(&process.name);
        process.executable_path = if include_paths {
            process
                .executable_path
                .as_ref()
                .map(|p| redact_path(p, home.as_deref()))
        } else {
            None
        };
        process.attribution.explanation = safe_text(&process.attribution.explanation);
        if let Some(app) = &mut process.attribution.application {
            app.name = safe_text(&app.name);
            app.bundle_path = if include_paths {
                redact_path(&app.bundle_path, home.as_deref())
            } else {
                "[path omitted]".into()
            };
        }
    }
    for group in &mut result.groups {
        group.name = safe_text(&group.name);
    }
    result
}

pub fn redact_path(path: &str, home: Option<&str>) -> String {
    let clean = safe_text(path);
    if let Some(home) = home {
        if clean == home {
            return "~".into();
        }
        if let Some(suffix) = clean.strip_prefix(&format!("{home}/")) {
            return format!("~/{suffix}");
        }
    }
    clean
}

pub fn inspect_text(snapshot: &Snapshot, id: &str) -> Result<String, String> {
    let ids = if let Some(group) = snapshot.groups.iter().find(|g| g.id == id) {
        group.process_ids.clone()
    } else if snapshot.processes.iter().any(|p| p.id == id) {
        vec![id.into()]
    } else {
        return Err("对象已退出、身份已变化，或 ID 不属于本次采样；请重新 list。".into());
    };
    let mut result = format!("采样时间：{} Unix ms\n", snapshot.sampled_at_unix_ms);
    for process in snapshot.processes.iter().filter(|p| ids.contains(&p.id)) {
        result.push_str(&format!("\n{} · PID {}\n对象 ID：{}\n内存：{} ({})\nCPU：{}\n身份：{:?}，开始 {}.{}\n路径：{}\n归属：{} / {}\n依据：{}\n保护：{}\n可用操作：只读；Alpha 未启用退出。\n",
            safe_text(&process.name), process.identity.pid, safe_text(&process.id),
            metric_bytes(&process.memory_bytes), safe_text(&process.metric_kind),
            process.cpu_one_core_percent.value.filter(|_| process.cpu_one_core_percent.status == Validity::Ok)
                .map(|v| format!("{v:.1}%（单核口径）")).unwrap_or_else(|| "— / 尚无有效时间差".into()),
            process.identity.status, process.identity.start_seconds.map(|v| v.to_string()).unwrap_or_else(|| "未知".into()),
            process.identity.start_microseconds.map(|v| format!("{v:06}")).unwrap_or_else(|| "未知".into()),
            process.executable_path.as_deref().map(safe_text).unwrap_or_else(|| "— / 不可读".into()),
            safe_text(&process.attribution.method), safe_text(&process.attribution.confidence),
            safe_text(&process.attribution.explanation), safe_text(&process.protection_reasons.join("；"))));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redaction_respects_path_boundaries() {
        assert_eq!(
            redact_path("/Users/test/project", Some("/Users/test")),
            "~/project"
        );
        assert_eq!(
            redact_path("/Users/tester/project", Some("/Users/test")),
            "/Users/tester/project"
        );
    }
    #[test]
    fn missing_metrics_never_look_like_zero() {
        assert!(
            metric_bytes(&Metric::<u64>::unavailable(
                Validity::Denied,
                "test",
                "denied"
            ))
            .contains("无权限")
        );
        assert_eq!(metric_bytes(&Metric::ok(0, "test")), "0.0 MiB");
    }
}
