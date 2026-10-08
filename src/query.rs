//! Read-only display queries. A match never changes grouping or identity.
use crate::model::{OccupancyGroup, ProcessInfo, Snapshot, Validity, safe_text};
use clap::ValueEnum;
use serde::Serialize;
use std::cmp::Ordering;

pub const MAX_SEARCH_CHARS: usize = 128;

/// Assign before display filtering so list, TUI and JSON use the same aliases.
/// FNV-1a-64 is fixed across Rust versions and invocations. The encoded full ID
/// only participates if even the entire hash collides; it keeps keys distinct.
pub fn assign_short_ids(groups: &mut [OccupancyGroup]) {
    let keys = groups
        .iter()
        .map(|group| {
            let hash = group.id.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
            let suffix: String = group.id.bytes().map(|byte| format!("{byte:02x}")).collect();
            format!("{hash:016x}{suffix}")
        })
        .collect::<Vec<_>>();
    for (group, short_id) in groups.iter_mut().zip(unique_prefixes(&keys)) {
        group.short_id = short_id;
    }
}

fn unique_prefixes(keys: &[String]) -> Vec<String> {
    let mut order: Vec<_> = (0..keys.len()).collect();
    order.sort_unstable_by_key(|&index| &keys[index]);
    let mut lengths = vec![8; keys.len()];
    for pair in order.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        let shared = keys[left]
            .bytes()
            .zip(keys[right].bytes())
            .take_while(|(a, b)| a == b)
            .count();
        lengths[left] = lengths[left].max(shared + 1);
        lengths[right] = lengths[right].max(shared + 1);
    }
    keys.iter()
        .zip(lengths)
        .map(|(key, length)| key[..length.min(key.len())].into())
        .collect()
}

#[derive(Debug)]
pub struct Inspection<'a> {
    pub group: Option<&'a OccupancyGroup>,
    pub processes: Vec<&'a ProcessInfo>,
}

/// Full IDs remain exact. A PID identifies whatever process has that PID now;
/// aliases are accepted only when exactly one current target matches.
pub fn resolve_inspect<'a>(snapshot: &'a Snapshot, input: &str) -> Result<Inspection<'a>, String> {
    let full_groups: Vec<_> = snapshot.groups.iter().filter(|g| g.id == input).collect();
    let full_processes: Vec<_> = snapshot
        .processes
        .iter()
        .filter(|p| p.id == input)
        .collect();
    let numeric = !input.is_empty() && input.bytes().all(|c| c.is_ascii_digit());
    let pid = numeric.then(|| input.parse::<u32>().ok()).flatten();
    let (groups, processes) = if !full_groups.is_empty() || !full_processes.is_empty() {
        // A UID-unreadable singleton group's full ID is also its member's ID.
        // Preserve the existing full-ID group inspection in that case.
        if full_groups.is_empty() {
            (full_groups, full_processes)
        } else {
            (full_groups, Vec::new())
        }
    } else {
        (
            snapshot
                .groups
                .iter()
                .filter(|g| !input.is_empty() && g.short_id == input)
                .collect(),
            snapshot
                .processes
                .iter()
                .filter(|p| pid == Some(p.identity.pid))
                .collect(),
        )
    };
    match groups.len() + processes.len() {
        0 => Err("The object exited, its identity changed, or its ID/PID is not in this sample; run list again.".into()),
        1 => {
            let group = groups.first().copied();
            let processes = if let Some(group) = group {
                snapshot.processes.iter().filter(|p| group.process_ids.contains(&p.id)).collect()
            } else { processes };
            Ok(Inspection { group, processes })
        }
        _ => Err("The PID or group ID is ambiguous in this sample; use a full ID from list --json.".into()),
    }
}

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
        ActivationPolicy, Application, Attribution, Category, Coverage, Metric, Pressure,
        ProcessIdentity, ProcessInfo, SCHEMA_VERSION, SystemMemory,
    };

    fn group(id: &str, name: &str, amount: u64) -> OccupancyGroup {
        OccupancyGroup {
            id: id.into(),
            short_id: String::new(),
            name: name.into(),
            category: Category::Application,
            memory_bytes: Metric::ok(amount),
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
                total_bytes: Metric::ok(1024),
                used_bytes: Metric::ok(1),
                compressed_bytes: Metric::ok(0),
                swap_used_bytes: Metric::ok(0),
                cached_bytes: Metric::ok(0),
                pressure: Metric::ok(Pressure::Normal),
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
                memory_bytes: Metric::ok(1),
                metric_kind: "rss".into(),
                cpu_one_core_percent: Metric::ok(0.0),
                category: Category::Application,
                attribution: Attribution {
                    application: Some(Application {
                        bundle_id: Some("org.example.Browser".into()),
                        bundle_path: "/private/secret-installation/Browser.app".into(),
                        name: "Browser".into(),
                        leader_pid: 12345,
                        frontmost: false,
                        activation_policy: ActivationPolicy::Regular,
                    }),
                    method: "fixture".into(),
                    confidence: "fixture".into(),
                    explanation: "secret-attribution".into(),
                },
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

    #[test]
    fn short_ids_are_stable_across_order_membership_and_display_filtering() {
        let mut groups = vec![
            group("app:p:boot:42:10:1", "App", 10),
            group("unattributed:path:501:abcd", "Tool", 20),
        ];
        assign_short_ids(&mut groups);
        assert!(groups.iter().all(|group| group.short_id.len() == 8));
        let original = groups.clone();
        groups.reverse();
        groups[0].process_ids.push("new-member".into());
        assign_short_ids(&mut groups);
        for group in &groups {
            assert_eq!(
                group.short_id,
                original
                    .iter()
                    .find(|old| old.id == group.id)
                    .unwrap()
                    .short_id
            );
        }
        groups.retain(|group| group.name == "App");
        assert_eq!(groups[0].short_id, original[0].short_id);
    }

    #[test]
    fn collisions_extend_every_affected_prefix_including_full_hash_collisions() {
        let keys = [
            "12345678abc00000aa",
            "12345678abc00000bb",
            "12345678d0000000cc",
            "fedcba9800000000dd",
        ]
        .map(String::from);
        assert_eq!(
            unique_prefixes(&keys),
            [
                "12345678abc00000a",
                "12345678abc00000b",
                "12345678d",
                "fedcba98"
            ]
        );
        let mut groups = vec![
            group("app:p:fixture:33095:10:1", "First", 1),
            group("app:p:fixture:55308:10:1", "Second", 2),
        ];
        assign_short_ids(&mut groups);
        assert_eq!(groups[0].short_id, "83d326af87");
        assert_eq!(groups[1].short_id, "83d326af88");
        // Filtering a collected sample must not reassign the formerly colliding alias.
        groups.truncate(1);
        assert_eq!(groups[0].short_id, "83d326af87");
    }

    #[test]
    fn inspect_rejects_ambiguous_aliases_and_never_rebinds_full_ids() {
        let mut snapshot = sample();
        assign_short_ids(&mut snapshot.groups);
        let full = snapshot.processes[0].id.clone();
        assert!(resolve_inspect(&snapshot, "12345").unwrap().group.is_none());
        assert!(
            resolve_inspect(&snapshot, &snapshot.groups[0].short_id)
                .unwrap()
                .group
                .is_some()
        );
        snapshot.processes[0].id = "replacement-member".into();
        assert!(
            resolve_inspect(&snapshot, &full)
                .unwrap_err()
                .contains("identity")
        );
        assert_eq!(
            resolve_inspect(&snapshot, "12345").unwrap().processes[0].id,
            "replacement-member"
        );
        snapshot.groups[0].short_id = "00012345".into();
        assert!(
            resolve_inspect(&snapshot, "00012345")
                .unwrap_err()
                .contains("ambiguous")
        );
        snapshot.groups[0].short_id = "abcdef12".into();
        let mut duplicate = snapshot.groups[0].clone();
        duplicate.id = "another-group".into();
        snapshot.groups.push(duplicate);
        assert!(
            resolve_inspect(&snapshot, "abcdef12")
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(resolve_inspect(&snapshot, "abcdef1").is_err());
        assert!(resolve_inspect(&snapshot, "").is_err());
    }
}
