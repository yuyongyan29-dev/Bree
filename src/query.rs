//! Read-only display queries. A match never changes grouping, policy or identity.
use crate::model::{OccupancyGroup, Snapshot, Validity, safe_text};
use clap::ValueEnum;
use serde::Serialize;
use std::cmp::Ordering;

pub const MAX_SEARCH_CHARS: usize = 128;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupSort {
    #[default]
    Memory,
    Name,
}

pub fn validate_search(value: &str) -> Result<String, String> {
    if value.chars().count() > MAX_SEARCH_CHARS {
        return Err(format!(
            "Search is limited to {MAX_SEARCH_CHARS} characters"
        ));
    }
    if safe_text(value) != value {
        return Err("Search cannot contain control or bidirectional formatting characters".into());
    }
    Ok(value.trim().to_string())
}

/// Search only displayed names, application bundle IDs and exact member PIDs.
/// Installation paths, attribution explanations and other private text are excluded.
pub fn group_matches(snapshot: &Snapshot, group: &OccupancyGroup, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    // Unreadable process names can themselves be "PID 12345". Numeric searches
    // must not find PID 12345 when the query is 1234.
    let numeric = query.chars().all(|c| c.is_ascii_digit());
    if !numeric && safe_text(&group.name).to_lowercase().contains(&query) {
        return true;
    }
    let pid = if numeric {
        query.parse::<u32>().ok()
    } else {
        None
    };
    snapshot.processes.iter().any(|process| {
        group.process_ids.contains(&process.id)
            && if numeric {
                pid == Some(process.identity.pid)
            } else {
                safe_text(&process.name).to_lowercase().contains(&query)
                    || process
                        .attribution
                        .application
                        .as_ref()
                        .and_then(|app| app.bundle_id.as_ref())
                        .is_some_and(|bundle| safe_text(bundle).to_lowercase().contains(&query))
            }
    })
}

/// Keep incomparable metric kinds separate and missing/invalid values after measured zero.
/// Stable identity breaks equal display-name ties so refresh cannot select a different instance.
pub fn compare_groups(left: &OccupancyGroup, right: &OccupancyGroup, sort: GroupSort) -> Ordering {
    let name_order = || {
        safe_text(&left.name)
            .to_lowercase()
            .cmp(&safe_text(&right.name).to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    };
    match sort {
        GroupSort::Name => name_order(),
        GroupSort::Memory => {
            let measured = |group: &OccupancyGroup| {
                (group.memory_bytes.status == Validity::Ok)
                    .then_some(group.memory_bytes.value)
                    .flatten()
            };
            let (left_value, right_value) = (measured(left), measured(right));
            left_value
                .is_none()
                .cmp(&right_value.is_none())
                .then_with(|| left.metric_kind.cmp(&right.metric_kind))
                .then_with(|| right_value.cmp(&left_value))
                .then_with(name_order)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Application, Attribution, Category, Coverage, Metric, Pressure, ProcessIdentity,
        ProcessInfo, SCHEMA_VERSION, SystemMemory,
    };

    fn group(id: &str, name: &str, amount: u64) -> OccupancyGroup {
        OccupancyGroup {
            id: id.into(),
            name: name.into(),
            category: Category::Application,
            memory_bytes: Metric::ok(amount, "fixture"),
            metric_kind: "rss".into(),
            process_ids: vec!["original-member".into()],
            explanation: "private-evidence".into(),
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            sampled_at_unix_ms: 1,
            collected_in_ms: 1,
            system: SystemMemory {
                total_bytes: Metric::ok(1024, "fixture"),
                used_bytes: Metric::ok(1, "fixture"),
                compressed_bytes: Metric::ok(0, "fixture"),
                swap_used_bytes: Metric::ok(0, "fixture"),
                cached_bytes: Metric::ok(0, "fixture"),
                pressure: Metric::ok(Pressure::Normal, "fixture"),
                used_definition: "fixture".into(),
            },
            processes: vec![ProcessInfo {
                id: "original-member".into(),
                identity: ProcessIdentity {
                    boot_session: "fixture-boot".into(),
                    pid: 12345,
                    start_seconds: Some(1),
                    start_microseconds: Some(2),
                    status: Validity::Ok,
                },
                parent_pid: None,
                uid: Some(501),
                name: "Browser Helper 中文".into(),
                executable_path: Some("/private/secret-location/executable".into()),
                memory_bytes: Metric::ok(1, "fixture"),
                metric_kind: "rss".into(),
                cpu_one_core_percent: Metric::ok(0.0, "fixture"),
                category: Category::Application,
                attribution: Attribution {
                    application: Some(Application {
                        bundle_id: Some("org.example.Browser".into()),
                        bundle_path: "/private/secret-installation/Browser.app".into(),
                        name: "Browser".into(),
                        leader_pid: 12345,
                        frontmost: false,
                    }),
                    method: "fixture".into(),
                    confidence: "fixture".into(),
                    explanation: "secret-attribution".into(),
                },
                protection_reasons: vec![],
                quit_supported: false,
            }],
            groups: vec![group("app:original", "Browser", 1)],
            coverage: Coverage {
                enumerated_processes: 1,
                readable_memory_processes: 1,
                reliable_identity_processes: 1,
                notes: vec![],
            },
            diagnostics: vec![],
        }
    }

    #[test]
    fn names_helpers_bundle_ids_and_unicode_match_without_scanning_private_text() {
        let snapshot = sample();
        let group = &snapshot.groups[0];
        for query in [
            "browser",
            "HELPER",
            "中文",
            "ORG.EXAMPLE",
            "  Browser  ",
            "",
        ] {
            assert!(group_matches(&snapshot, group, query), "{query}");
        }
        for query in [
            "secret-location",
            "secret-installation",
            "secret-attribution",
            "private-evidence",
            "missing",
        ] {
            assert!(!group_matches(&snapshot, group, query), "{query}");
        }
    }

    #[test]
    fn numeric_search_matches_the_exact_member_pid_and_not_a_pid_prefix() {
        let mut snapshot = sample();
        snapshot.groups[0].name = "PID 12345".into();
        snapshot.processes[0].name = "PID 12345".into();
        assert!(group_matches(&snapshot, &snapshot.groups[0], "12345"));
        assert!(group_matches(&snapshot, &snapshot.groups[0], "0012345"));
        assert!(!group_matches(&snapshot, &snapshot.groups[0], "1234"));
        assert!(!group_matches(
            &snapshot,
            &snapshot.groups[0],
            "99999999999999999999"
        ));
    }

    #[test]
    fn replaced_process_id_does_not_match_the_frozen_group_members() {
        let mut snapshot = sample();
        snapshot.processes[0].id = "replacement-member".into();
        assert!(!group_matches(&snapshot, &snapshot.groups[0], "helper"));
        assert!(!group_matches(&snapshot, &snapshot.groups[0], "12345"));
    }

    #[test]
    fn search_validation_counts_unicode_characters_and_rejects_control_injection() {
        assert_eq!(validate_search("  Browser  ").unwrap(), "Browser");
        assert!(validate_search(&"中".repeat(MAX_SEARCH_CHARS)).is_ok());
        assert!(validate_search(&"中".repeat(MAX_SEARCH_CHARS + 1)).is_err());
        for query in ["x\n", "x\t", "\u{1b}[31m", "x\u{202e}y", "x\u{2066}y"] {
            assert!(validate_search(query).is_err());
        }
    }

    #[test]
    fn memory_sort_keeps_invalid_values_after_zero_without_mixing_metrics() {
        let zero = group("zero", "Zero", 0);
        let mut missing = group("missing", "Missing", u64::MAX);
        missing.memory_bytes.status = Validity::Denied;
        assert_eq!(
            compare_groups(&zero, &missing, GroupSort::Memory),
            Ordering::Less
        );
        missing.memory_bytes.value = None;
        assert_eq!(
            compare_groups(&zero, &missing, GroupSort::Memory),
            Ordering::Less
        );
        let mut other_metric = group("other", "Other", u64::MAX);
        other_metric.metric_kind = "virtual".into();
        assert_eq!(
            compare_groups(&zero, &other_metric, GroupSort::Memory),
            Ordering::Less
        );
    }

    #[test]
    fn sorting_is_case_insensitive_and_deterministic_for_equal_names_and_amounts() {
        let a = group("a", "alpha", 100);
        let b = group("b", "Zulu", 200);
        assert_eq!(compare_groups(&a, &b, GroupSort::Name), Ordering::Less);
        assert_eq!(compare_groups(&a, &b, GroupSort::Memory), Ordering::Greater);
        let mut equal = a.clone();
        equal.id = "z".into();
        assert_eq!(compare_groups(&a, &equal, GroupSort::Name), Ordering::Less);
        assert_eq!(
            compare_groups(&a, &equal, GroupSort::Memory),
            Ordering::Less
        );
    }
}
