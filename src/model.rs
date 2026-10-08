use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validity {
    Ok,
    Denied,
    Unsupported,
    Exited,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric<T> {
    pub value: Option<T>,
    pub status: Validity,
    pub source: String,
    pub reason: Option<String>,
}

impl<T> Metric<T> {
    pub fn ok(value: T, source: impl Into<String>) -> Self {
        Self {
            value: Some(value),
            status: Validity::Ok,
            source: source.into(),
            reason: None,
        }
    }
    pub fn unavailable(
        status: Validity,
        source: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            value: None,
            status,
            source: source.into(),
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    Normal,
    Elevated,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMemory {
    pub total_bytes: Metric<u64>,
    pub used_bytes: Metric<u64>,
    pub compressed_bytes: Metric<u64>,
    pub swap_used_bytes: Metric<u64>,
    pub cached_bytes: Metric<u64>,
    pub pressure: Metric<Pressure>,
    pub used_definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub boot_session: String,
    pub pid: u32,
    pub start_seconds: Option<u64>,
    pub start_microseconds: Option<u32>,
    pub status: Validity,
}

impl ProcessIdentity {
    pub fn object_id(&self) -> String {
        format!(
            "p:{}:{}:{}:{}",
            self.boot_session,
            self.pid,
            self.start_seconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            self.start_microseconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into())
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Application,
    System,
    Unknown,
}

/// AppKit activation policy as read from the running application.
/// Only `Regular` (an ordinary Dock app) can lead an application group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPolicy {
    Regular,
    Accessory,
    Prohibited,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Application {
    pub bundle_id: Option<String>,
    pub bundle_path: String,
    pub name: String,
    pub leader_pid: u32,
    pub frontmost: bool,
    pub activation_policy: ActivationPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attribution {
    pub application: Option<Application>,
    pub method: String,
    pub confidence: String,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub id: String,
    pub identity: ProcessIdentity,
    pub parent_pid: Option<u32>,
    pub uid: Option<u32>,
    pub name: String,
    pub executable_path: Option<String>,
    pub memory_bytes: Metric<u64>,
    pub metric_kind: String,
    pub cpu_one_core_percent: Metric<f64>,
    pub category: Category,
    pub attribution: Attribution,
}

impl ProcessInfo {
    pub(crate) fn reliable_identity(&self) -> bool {
        self.identity.status == Validity::Ok
            && self.identity.pid > 1
            && !self.identity.boot_session.is_empty()
            && !self.identity.boot_session.chars().any(char::is_control)
            && self
                .identity
                .start_seconds
                .is_some_and(|seconds| seconds > 0)
            && self
                .identity
                .start_microseconds
                .is_some_and(|micros| micros < 1_000_000)
            && self.id == self.identity.object_id()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OccupancyGroup {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub memory_bytes: Metric<u64>,
    pub metric_kind: String,
    pub process_ids: Vec<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub enumerated_processes: usize,
    pub readable_memory_processes: usize,
    pub reliable_identity_processes: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub sampled_at_unix_ms: u64,
    pub collected_in_ms: u64,
    pub system: SystemMemory,
    pub processes: Vec<ProcessInfo>,
    pub groups: Vec<OccupancyGroup>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}

/// Untrusted process text must never inject terminal controls or bidi overrides.
pub fn safe_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_changes_after_pid_reuse() {
        let a = ProcessIdentity {
            boot_session: "boot".into(),
            pid: 42,
            start_seconds: Some(10),
            start_microseconds: Some(1),
            status: Validity::Ok,
        };
        let mut b = a.clone();
        b.start_microseconds = Some(2);
        assert_ne!(a.object_id(), b.object_id());
        b = a.clone();
        b.boot_session = "next-boot".into();
        assert_ne!(a.object_id(), b.object_id());
    }
    #[test]
    fn zero_and_missing_are_distinct() {
        assert_eq!(Metric::ok(0_u64, "test").value, Some(0));
        assert_eq!(
            Metric::<u64>::unavailable(Validity::Denied, "test", "denied").value,
            None
        );
    }
    #[test]
    fn removes_terminal_controls() {
        let result = safe_text("名字\u{1b}[31m\n\u{202e}路径");
        assert!(!result.contains('\u{1b}'));
        assert!(!result.contains('\u{202e}'));
        assert!(result.contains("名字"));
    }
}
