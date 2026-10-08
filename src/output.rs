use crate::model::{Category, Metric, Pressure, Snapshot, Validity, safe_text};
use crate::query::Inspection;
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
        (status, _) => format!("— / {}", missing_text(status)),
    }
}

/// Text output keeps each missing cause distinct; none of them is a zero value.
pub fn missing_text(status: Validity) -> &'static str {
    match status {
        Validity::Denied => "Permission denied",
        Validity::Unsupported => "Unsupported",
        Validity::Exited => "Exited",
        Validity::Stale => "Stale",
        // An Ok status without a value is malformed, so it is reported as unknown.
        Validity::Unknown | Validity::Ok => "Unknown",
    }
}

pub fn pressure_text(metric: &Metric<Pressure>) -> String {
    match (metric.status, metric.value) {
        (Validity::Ok, Some(Pressure::Normal)) => "Normal".into(),
        (Validity::Ok, Some(Pressure::Elevated)) => "Elevated".into(),
        (Validity::Ok, Some(Pressure::High)) => "High".into(),
        _ => "Unknown".into(),
    }
}

pub fn category_text(category: Category) -> &'static str {
    match category {
        Category::Application => "Application",
        Category::System => "System",
        Category::Unknown => "Unattributed",
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

pub fn local_time(unix_ms: u64) -> String {
    let Ok(seconds) = libc::time_t::try_from(unix_ms / 1_000) else {
        return "Unknown".into();
    };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers refer to valid storage; the output is only read
    // after localtime_r reports success. No process-global time buffer is used.
    if unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null() {
        return "Unknown".into();
    }
    let local = unsafe { local.assume_init() };
    format!(
        "{:02}:{:02}:{:02}",
        local.tm_hour, local.tm_min, local.tm_sec
    )
}

pub fn status_text(snapshot: &Snapshot) -> String {
    let memory = &snapshot.system;
    format!(
        "bree · Read-only Alpha\nMemory pressure: {}\nUsed {} / Total {}\nCompressed {} · Swap {}\nSampled at: {} · Collection {} ms\nCoverage: {} enumerated, {} readable memory, {} reliable identities\nApp-group totals do not equal reclaimable memory.",
        pressure_text(&memory.pressure),
        metric_bytes(&memory.used_bytes),
        metric_bytes(&memory.total_bytes),
        metric_bytes(&memory.compressed_bytes),
        metric_bytes(&memory.swap_used_bytes),
        local_time(snapshot.sampled_at_unix_ms),
        snapshot.collected_in_ms,
        snapshot.coverage.enumerated_processes,
        snapshot.coverage.readable_memory_processes,
        snapshot.coverage.reliable_identity_processes
    )
}

pub fn list_text(snapshot: &Snapshot, limit: Option<usize>) -> String {
    let mut text = status_text(snapshot);
    text.push_str("\n\nMemory\tInstances\tCategory\tName\tID\n");
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
            safe_text(&group.short_id)
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

pub fn inspect_text(snapshot: &Snapshot, inspection: &Inspection<'_>) -> String {
    let mut result = String::new();
    if let Some(group) = inspection.group {
        result.push_str(&format!(
            "{} · {} instances · Total {} · ID {}\n",
            safe_text(&group.name),
            group.process_ids.len(),
            metric_bytes(&group.memory_bytes),
            safe_text(&group.short_id)
        ));
    }
    result.push_str(&format!(
        "Sampled at: {}\n",
        local_time(snapshot.sampled_at_unix_ms)
    ));
    for process in &inspection.processes {
        result.push_str(&format!("\n{} · PID {}\nMemory: {} ({})\nCPU: {}\nIdentity: {:?}, started {}.{}\nPath: {}\nAttribution: {} / {}\nEvidence: {}\n",
            safe_text(&process.name), process.identity.pid,
            metric_bytes(&process.memory_bytes), safe_text(&process.metric_kind),
            process.cpu_one_core_percent.value.filter(|_| process.cpu_one_core_percent.status == Validity::Ok)
                .map(|v| format!("{v:.1}% (one-core basis)")).unwrap_or_else(|| "— / No valid sampling interval yet".into()),
            process.identity.status, process.identity.start_seconds.map(|v| v.to_string()).unwrap_or_else(|| "Unknown".into()),
            process.identity.start_microseconds.map(|v| format!("{v:06}")).unwrap_or_else(|| "Unknown".into()),
            process.executable_path.as_deref().map(safe_text).unwrap_or_else(|| "— / Unreadable".into()),
            safe_text(&process.attribution.method), safe_text(&process.attribution.confidence),
            safe_text(&process.attribution.explanation)));
    }
    result
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
            metric_bytes(&Metric::<u64>::unavailable(Validity::Denied, "denied"))
                .contains("Permission denied")
        );
        assert_eq!(metric_bytes(&Metric::ok(0)), "0.0 MiB");
    }
    #[test]
    fn each_missing_cause_has_its_own_text() {
        let text = |status| metric_bytes(&Metric::<u64>::unavailable(status, "missing"));
        assert_eq!(text(Validity::Denied), "— / Permission denied");
        assert_eq!(text(Validity::Unsupported), "— / Unsupported");
        assert_eq!(text(Validity::Exited), "— / Exited");
        assert_eq!(text(Validity::Stale), "— / Stale");
        assert_eq!(text(Validity::Unknown), "— / Unknown");
        let malformed = Metric::<u64> {
            value: None,
            status: Validity::Ok,
            reason: None,
        };
        assert_eq!(metric_bytes(&malformed), "— / Unknown");
    }
}
