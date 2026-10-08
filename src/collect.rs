//! Read-only sampling. Object identifiers describe samples, never control handles.
use crate::model::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
#[path = "platform/macos.rs"]
mod platform;
#[cfg(not(target_os = "macos"))]
#[path = "platform/unsupported.rs"]
mod platform;

pub const PROCESS_METRIC_KIND: &str = "rss";

pub(crate) struct RawProcess {
    pub identity: ProcessIdentity,
    pub parent_pid: Option<u32>,
    pub uid: Option<u32>,
    pub name: String,
    pub executable_path: Option<String>,
    pub memory_bytes: Metric<u64>,
    pub cpu_total_ns: Option<u64>,
    pub sampled_at: Instant,
}

#[derive(Clone)]
pub(crate) struct AppEvidence {
    pub app: Application,
    pub leader_identity: ProcessIdentity,
    pub executable_path: String,
    pub leader_uid: u32,
}

/// Initialize on the main thread before moving into a sampling worker.
/// The collector owns only plain Rust data; AppKit objects never cross threads.
pub struct Collector {
    platform: platform::Backend,
    cpu_baselines: HashMap<String, (u64, Instant)>,
}

/// Give AppKit's main run loop one bounded turn before requesting a refresh.
/// No sampling, application activation, or process action is performed here.
pub fn pump_platform_events() {
    platform::pump_events();
}

impl Collector {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            platform: platform::Backend::new()?,
            cpu_baselines: HashMap::new(),
        })
    }

    pub fn snapshot(&mut self) -> Result<Snapshot, String> {
        let started = Instant::now();
        let sampled_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("System clock is before the Unix epoch: {e}"))?
            .as_millis() as u64;
        let system = self.platform.system_memory();
        if system.total_bytes.value.is_none() || system.used_bytes.value.is_none() {
            return Err(format!(
                "Core system memory metrics are unreadable: total={}; used={}",
                system.total_bytes.reason.as_deref().unwrap_or("ok"),
                system.used_bytes.reason.as_deref().unwrap_or("ok")
            ));
        }
        let (raw_processes, mut diagnostics) = self.platform.processes()?;
        let apps = main_applications(self.platform.applications(), &raw_processes);
        let mut next_baselines = HashMap::with_capacity(raw_processes.len());
        let mut processes: Vec<ProcessInfo> = raw_processes
            .into_iter()
            .map(|p| {
                let id = p.identity.object_id();
                let attribution = attribute(&p, &apps);
                let cpu = match (p.cpu_total_ns, p.identity.status) {
                    (Some(total), Validity::Ok) => {
                        let value =
                            cpu_between(self.cpu_baselines.get(&id).copied(), total, p.sampled_at);
                        next_baselines.insert(id.clone(), (total, p.sampled_at));
                        value
                    }
                    _ => Metric::unavailable(
                        if p.memory_bytes.status == Validity::Ok {
                            Validity::Unknown
                        } else {
                            p.memory_bytes.status
                        },
                        "Identity or cumulative CPU time is unreadable",
                    ),
                };
                let (category, _) = classify_process(&p, &attribution);
                ProcessInfo {
                    id,
                    identity: p.identity,
                    parent_pid: p.parent_pid,
                    uid: p.uid,
                    name: safe_text(&p.name),
                    executable_path: p.executable_path.map(|v| safe_text(&v)),
                    memory_bytes: p.memory_bytes,
                    metric_kind: PROCESS_METRIC_KIND.into(),
                    cpu_one_core_percent: cpu,
                    category,
                    attribution,
                }
            })
            .collect();
        self.cpu_baselines = next_baselines;
        processes.sort_by(|a, b| {
            b.memory_bytes
                .value
                .cmp(&a.memory_bytes.value)
                .then(a.id.cmp(&b.id))
        });
        let groups = occupancy_groups(&processes);
        let coverage = Coverage {
            enumerated_processes: processes.len(),
            readable_memory_processes: processes
                .iter()
                .filter(|p| p.memory_bytes.status == Validity::Ok)
                .count(),
            reliable_identity_processes: processes
                .iter()
                .filter(|p| p.identity.status == Validity::Ok)
                .count(),
            notes: vec![
                "All processes use RSS; shared pages may appear in multiple processes. App-group totals do not equal reclaimable memory."
                    .into(),
                "Read failures retain missing status; only metrics with the same complete instance identity enter the sample.".into(),
                "App attribution uses the AppKit main app and executable paths in the same bundle; AI tasks are not inferred from names or parent chains."
                    .into(),
                "Dynamic app information requires main-thread run-loop updates; foreground markers are observations only, and all actions are disabled.".into(),
            ],
        };
        if coverage.readable_memory_processes < coverage.enumerated_processes {
            diagnostics.push(format!(
                "Memory coverage: {}/{} processes; see per-process status for missing data.",
                coverage.readable_memory_processes, coverage.enumerated_processes
            ));
        }
        Ok(Snapshot {
            schema_version: SCHEMA_VERSION,
            sampled_at_unix_ms,
            collected_in_ms: started.elapsed().as_millis() as u64,
            system,
            processes,
            groups,
            coverage,
            diagnostics,
        })
    }
}

fn cpu_between(previous: Option<(u64, Instant)>, total_ns: u64, now: Instant) -> Metric<f64> {
    let Some((previous_total, previous_time)) = previous else {
        return Metric::unavailable(
            Validity::Unknown,
            "The first sample establishes a baseline; the next sample needs a real time interval",
        );
    };
    let Some(delta_cpu) = total_ns.checked_sub(previous_total) else {
        return Metric::unavailable(
            Validity::Stale,
            "Cumulative CPU time decreased; rebuilding the baseline",
        );
    };
    let elapsed = now.saturating_duration_since(previous_time);
    if elapsed.as_secs() >= 30 {
        return Metric::unavailable(
            Validity::Unknown,
            "Sampling interval reached 30 seconds; rebuilding the CPU baseline after wake or prolonged inactivity",
        );
    }
    let elapsed_ns = elapsed.as_nanos();
    if elapsed_ns == 0 {
        return Metric::unavailable(Validity::Unknown, "Sampling interval is zero");
    }
    Metric::ok(delta_cpu as f64 / elapsed_ns as f64 * 100.0)
}

/// Only ordinary (Regular activation policy) apps in the current sample lead a group.
/// Accessory/background apps and nested helper bundles that AppKit also lists
/// stay out, so their processes resolve to the enclosing main app's installation.
fn main_applications(apps: Vec<AppEvidence>, processes: &[RawProcess]) -> Vec<AppEvidence> {
    apps.into_iter()
        .filter(|app| {
            app.app.activation_policy == ActivationPolicy::Regular
                && processes.iter().any(|p| p.identity == app.leader_identity)
        })
        .collect()
}

fn is_system_executable(path: &str) -> bool {
    ["/System", "/usr/libexec", "/usr/sbin", "/sbin"]
        .iter()
        .any(|root| Path::new(path).starts_with(root))
}

/// Display category and system protection are independent: macOS's bundled
/// user applications belong in the application filter while retaining the
/// conservative system protection associated with their installation path.
fn classify_process(process: &RawProcess, attribution: &Attribution) -> (Category, bool) {
    let system_object = process.uid == Some(0)
        || process.identity.pid <= 1
        || process
            .executable_path
            .as_deref()
            .is_some_and(is_system_executable);
    let ordinary_system_app = attribution
        .application
        .as_ref()
        .is_some_and(|app| Path::new(&app.bundle_path).starts_with("/System/Applications"));
    let category = if attribution.application.is_some() && (!system_object || ordinary_system_app) {
        Category::Application
    } else if system_object {
        Category::System
    } else {
        Category::Unknown
    };
    (category, system_object)
}

fn attribute(process: &RawProcess, apps: &[AppEvidence]) -> Attribution {
    let unknown = |reason: &str| Attribution {
        application: None,
        method: "unattributed".into(),
        confidence: "unknown".into(),
        explanation: reason.into(),
    };
    if process.identity.status != Validity::Ok {
        return unknown("Exact instance identity is invalid; attribution is unconfirmed.");
    }
    let Some(path) = process.executable_path.as_deref() else {
        return unknown(
            "Executable path is unreadable; attribution is not inferred from the name or parent process.",
        );
    };
    let has_direct_app = apps
        .iter()
        .any(|e| e.app.leader_pid == process.identity.pid);
    let candidates: Vec<_> = apps
        .iter()
        .filter(|e| {
            if process.uid != Some(e.leader_uid) {
                return false;
            }
            if e.app.leader_pid == process.identity.pid {
                e.leader_identity == process.identity && e.executable_path == path
            } else if !has_direct_app {
                // Same installation evidence only. Different installed copies cannot collide.
                Path::new(path).starts_with(Path::new(&e.app.bundle_path).join("Contents"))
            } else {
                false
            }
        })
        .collect();
    if candidates.len() != 1 {
        return unknown(if candidates.is_empty() {
            "No verified main app and same-bundle path evidence; shown separately."
        } else {
            "Attribution conflicts between running instances or nested bundles; shown separately without double counting."
        });
    }
    let candidate = candidates[0];
    let direct = candidate.app.leader_pid == process.identity.pid;
    Attribution { application: Some(candidate.app.clone()),
        method: if direct { "appkit_main_application" } else { "same_bundle_executable" }.into(),
        confidence: if direct { "high" } else { "installation_evidence" }.into(),
        explanation: if direct { "The AppKit main app PID, executable path, and instance identity with microsecond precision match." }
            else { "The executable is within bundle/Contents of the verified running main app; this only establishes installation attribution, not sharing or permission to stop." }.into(),
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum OccupancyKey {
    Application(String),
    Executable(u32, String),
    Name(u32, String),
    Instance(String),
}

impl OccupancyKey {
    fn id(&self) -> String {
        match self {
            Self::Application(anchor) => format!("app:{anchor}"),
            Self::Executable(uid, path) => unattributed_id("path", *uid, path),
            Self::Name(uid, name) => unattributed_id("name", *uid, name),
            Self::Instance(id) => id.clone(),
        }
    }
}

fn unattributed_id(kind: &str, uid: u32, value: &str) -> String {
    // Fixed FNV-1a-128 keeps IDs repeatable across samples, invocations and Rust
    // versions without exporting paths. Grouping uses the full key, not a hash.
    let fingerprint = value
        .bytes()
        .fold(0x6c62272e07bb014262b821756295c58d_u128, |hash, byte| {
            (hash ^ u128::from(byte)).wrapping_mul(0x0000000001000000000000000000013b)
        });
    format!("unattributed:{kind}:{uid}:{fingerprint:032x}")
}

fn occupancy_groups(processes: &[ProcessInfo]) -> Vec<OccupancyGroup> {
    let mut groups: BTreeMap<OccupancyKey, (OccupancyGroup, bool)> = BTreeMap::new();
    let mut seen = HashSet::new();
    for process in processes {
        if !seen.insert(process.id.clone()) {
            continue;
        }
        let label = process
            .attribution
            .application
            .is_none()
            .then(|| crate::attribution::labeled_process_name(process))
            .flatten();
        let (key, name, explanation) = if let Some(app) = &process.attribution.application {
            let leader = processes.iter().find(|p| p.identity.pid == app.leader_pid);
            let anchor = leader.map(|p| p.id.as_str()).unwrap_or(process.id.as_str());
            (
                OccupancyKey::Application(anchor.into()),
                app.name.clone(),
                "RSS total of the same running main app and processes in its bundle; each instance is counted once, and missing values are not replaced with zero.".into(),
            )
        } else {
            let (key, explanation) = match (process.uid, process.executable_path.as_deref().filter(|path| !path.is_empty())) {
                (Some(uid), Some(path)) => (
                    OccupancyKey::Executable(uid, path.into()),
                    "Unattributed: grouped by the same executable path and UID, without establishing application ownership. Each instance is counted once; missing RSS makes the total unknown.".into(),
                ),
                (Some(uid), None) => (
                    OccupancyKey::Name(uid, process.name.clone()),
                    "Unattributed: executable paths are unreadable; grouped only by the same process name and UID, without establishing application ownership or a shared executable. Each instance is counted once; missing RSS makes the total unknown.".into(),
                ),
                (None, _) => (
                    OccupancyKey::Instance(process.id.clone()),
                    "Unattributed: UID is unreadable; this instance is shown separately because a shared user cannot be established.".into(),
                ),
            };
            (
                key,
                safe_text(label.as_deref().unwrap_or(&process.name)),
                explanation,
            )
        };
        let id = key.id();
        let (group, has_label) = groups.entry(key).or_insert_with(|| {
            (
                OccupancyGroup {
                    id,
                    name: name.clone(),
                    category: process.category,
                    memory_bytes: process.memory_bytes.clone(),
                    metric_kind: PROCESS_METRIC_KIND.into(),
                    process_ids: Vec::new(),
                    explanation,
                },
                label.is_some(),
            )
        });
        // A later member may have a reliable developer label even if the first
        // member does not. RSS/input order must not determine the display name.
        if (label.is_some() && !*has_label) || (label.is_some() == *has_label && name < group.name)
        {
            group.name = name;
            *has_label = label.is_some();
        }
        if process.attribution.application.is_none() && process.category == Category::System {
            group.category = Category::System;
        }
        let first_member = group.process_ids.is_empty();
        group.process_ids.push(process.id.clone());
        if first_member {
            // A one-process group is that exact metric, including its denial,
            // exit, unsupported status, and reason. Only an incomplete
            // aggregate requires an aggregate-level unknown value.
            continue;
        }
        match (group.memory_bytes.value, process.memory_bytes.value) {
            (Some(sum), Some(value)) if process.memory_bytes.status == Validity::Ok => {
                match sum.checked_add(value) {
                    Some(total) => group.memory_bytes = Metric::ok(total),
                    None => {
                        group.memory_bytes =
                            Metric::unavailable(Validity::Unknown, "Group total overflow")
                    }
                }
            }
            _ => {
                group.memory_bytes = Metric::unavailable(
                    Validity::Unknown,
                    "At least one instance in the group has missing memory; the total is unknown, see individual processes",
                )
            }
        }
    }
    let mut groups: Vec<_> = groups.into_values().map(|(group, _)| group).collect();
    groups.sort_by(|a, b| {
        b.memory_bytes
            .value
            .cmp(&a.memory_bytes.value)
            .then(a.id.cmp(&b.id))
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn raw(pid: u32, path: Option<&str>) -> RawProcess {
        RawProcess {
            identity: ProcessIdentity {
                boot_session: "boot".into(),
                pid,
                start_seconds: Some(42),
                start_microseconds: Some(pid),
                status: Validity::Ok,
            },
            parent_pid: None,
            uid: Some(501),
            name: "node".into(),
            executable_path: path.map(str::to_string),
            memory_bytes: Metric::ok(10),
            cpu_total_ns: Some(0),
            sampled_at: Instant::now(),
        }
    }
    fn app(pid: u32, bundle: &str) -> AppEvidence {
        AppEvidence {
            app: Application {
                bundle_id: Some("com.example.Test".into()),
                bundle_path: bundle.into(),
                name: "Test".into(),
                leader_pid: pid,
                frontmost: false,
                activation_policy: ActivationPolicy::Regular,
            },
            executable_path: format!("{bundle}/Contents/MacOS/Test"),
            leader_uid: 501,
            leader_identity: raw(pid, None).identity,
        }
    }
    fn accessory(pid: u32, bundle: &str) -> AppEvidence {
        let mut evidence = app(pid, bundle);
        evidence.app.activation_policy = ActivationPolicy::Accessory;
        evidence
    }
    fn info(raw: RawProcess, attribution: Attribution) -> ProcessInfo {
        ProcessInfo {
            id: raw.identity.object_id(),
            identity: raw.identity,
            parent_pid: None,
            uid: raw.uid,
            name: raw.name,
            executable_path: raw.executable_path,
            memory_bytes: raw.memory_bytes,
            metric_kind: PROCESS_METRIC_KIND.into(),
            cpu_one_core_percent: Metric::unavailable(Validity::Unknown, "baseline"),
            category: Category::Application,
            attribution,
        }
    }

    fn unattributed(process: RawProcess) -> ProcessInfo {
        let attribution = attribute(&process, &[]);
        let category = classify_process(&process, &attribution).0;
        let mut result = info(process, attribution);
        result.category = category;
        result
    }

    #[test]
    fn unattributed_same_path_and_uid_merge_distinct_instances_once() {
        for (path, category) in [
            ("/opt/example/bin/tool", Category::Unknown),
            ("/usr/libexec/example", Category::System),
        ] {
            let first = unattributed(raw(42, Some(path)));
            let mut second = unattributed(raw(43, Some(path)));
            second.name = "different process name".into();
            let groups = occupancy_groups(&[first.clone(), second.clone(), first.clone()]);
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].category, category);
            assert_eq!(groups[0].memory_bytes.value, Some(20));
            assert_eq!(groups[0].process_ids, vec![first.id, second.id]);
            assert!(groups[0].explanation.starts_with("Unattributed:"));
            assert!(second.attribution.application.is_none());
            assert_eq!(second.attribution.method, "unattributed");
        }
    }

    #[test]
    fn unreadable_paths_merge_by_name_and_uid_without_joining_known_paths() {
        let first = unattributed(raw(42, None));
        let second = unattributed(raw(43, None));
        let mut other_name = unattributed(raw(44, None));
        other_name.name = "python".into();
        let known_path = unattributed(raw(45, Some("/opt/bin/node")));
        let other_path = unattributed(raw(46, Some("/opt/other/node")));
        let groups = occupancy_groups(&[
            first.clone(),
            second.clone(),
            other_name,
            known_path,
            other_path,
        ]);
        assert_eq!(groups.len(), 4);
        let merged = groups.iter().find(|g| g.process_ids.len() == 2).unwrap();
        assert_eq!(merged.process_ids, vec![first.id, second.id]);
        assert_eq!(merged.memory_bytes.value, Some(20));
        assert!(merged.explanation.contains("paths are unreadable"));

        // Path and name keys must stay distinct even when the strings coincide.
        let known = unattributed(raw(50, Some("/opt/bin/node")));
        let mut named = unattributed(raw(51, None));
        named.name = "/opt/bin/node".into();
        let groups = occupancy_groups(&[known, named]);
        assert_eq!(groups.len(), 2);
        assert_ne!(groups[0].id, groups[1].id);
    }

    #[test]
    fn different_or_unreadable_uids_never_merge() {
        for path in [Some("/opt/bin/node"), None] {
            let first = unattributed(raw(42, path));
            let mut other_uid = unattributed(raw(43, path));
            other_uid.uid = Some(502);
            let mut unknown_uid = unattributed(raw(44, path));
            unknown_uid.uid = None;
            let mut other_unknown_uid = unattributed(raw(45, path));
            other_unknown_uid.uid = None;
            let groups = occupancy_groups(&[first, other_uid, unknown_uid, other_unknown_uid]);
            assert_eq!(groups.len(), 4);
            assert!(groups.iter().all(|g| g.process_ids.len() == 1));
            assert_eq!(
                groups.iter().map(|g| &g.id).collect::<HashSet<_>>().len(),
                4
            );
        }
    }

    #[test]
    fn unattributed_group_ids_and_names_survive_reordering_and_member_turnover() {
        for path in [Some("/Users/example/private/tool"), None] {
            let first = unattributed(raw(42, path));
            let mut second = unattributed(raw(43, path));
            second.memory_bytes = Metric::ok(20);
            if path.is_some() {
                second.name = "alternate name".into();
            }
            let forward = occupancy_groups(&[first.clone(), second.clone()]);
            let reverse = occupancy_groups(&[second.clone(), first]);
            assert_eq!(forward[0].id, reverse[0].id);
            assert_eq!(forward[0].name, reverse[0].name);
            assert_eq!(forward[0].id, occupancy_groups(&[second])[0].id);

            let mut replacement = unattributed(raw(90, path));
            replacement.identity.start_seconds = Some(100);
            replacement.id = replacement.identity.object_id();
            let replacement_id = occupancy_groups(&[replacement.clone()])[0].id.clone();
            assert_eq!(forward[0].id, replacement_id);
            assert!(!replacement_id.contains("/Users"));
            assert!(!replacement_id.contains("private"));
            replacement.uid = Some(502);
            assert_ne!(forward[0].id, occupancy_groups(&[replacement])[0].id);
        }
    }

    #[test]
    fn developer_labels_name_unattributed_groups_but_not_application_groups() {
        let path = "/Users/test/.local/share/claude/versions/2.1.294";
        let mut candidate = raw(42, Some(path));
        candidate.uid = Some(unsafe { libc::geteuid() });
        candidate.name = "2.1.294".into();
        let labeled = unattributed(candidate);
        let mut unverified = labeled.clone();
        unverified.identity.pid = 43;
        unverified.identity.status = Validity::Stale;
        unverified.id = unverified.identity.object_id();
        for processes in [
            vec![unverified.clone(), labeled.clone()],
            vec![labeled.clone(), unverified],
        ] {
            let groups = occupancy_groups(&processes);
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].name, "Claude Code 2.1.294");
            assert!(groups[0].explanation.starts_with("Unattributed:"));
        }
        let mut other_version = labeled.clone();
        other_version.identity.pid = 44;
        other_version.id = other_version.identity.object_id();
        other_version.executable_path =
            Some("/Users/test/.local/share/claude/versions/3.0.0".into());
        let groups = occupancy_groups(&[labeled.clone(), other_version]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().any(|g| g.name == "Claude Code 3.0.0"));

        let mut application = labeled;
        application.attribution.application = Some(app(42, "/Applications/Test.app").app);
        let groups = occupancy_groups(&[application.clone()]);
        assert_eq!(groups[0].name, "Test");
        assert_eq!(groups[0].id, format!("app:{}", application.id));
    }

    #[test]
    fn unattributed_totals_retain_unknown_memory_and_system_category() {
        let mut system = unattributed(raw(42, Some("/opt/bin/node")));
        system.category = Category::System;
        let mut missing = unattributed(raw(43, Some("/opt/bin/node")));
        missing.memory_bytes = Metric::unavailable(Validity::Denied, "denied");
        for processes in [vec![system.clone(), missing.clone()], vec![missing, system]] {
            let groups = occupancy_groups(&processes);
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].category, Category::System);
            assert_eq!(groups[0].memory_bytes.status, Validity::Unknown);
            assert_eq!(groups[0].memory_bytes.value, None);
        }
    }

    #[test]
    fn app_copies_and_unknown_node_stay_separate() {
        let apps = vec![
            app(1, "/Applications/Test.app"),
            app(2, "/Users/x/Test.app"),
        ];
        let helper = raw(3, Some("/Applications/Test.app/Contents/Frameworks/Helper"));
        assert_eq!(attribute(&helper, &apps).application.unwrap().leader_pid, 1);
        assert!(
            attribute(
                &raw(4, Some("/Applications/Test.app-copy/Contents/Helper")),
                &apps
            )
            .application
            .is_none()
        );
        assert!(
            attribute(&raw(5, Some("/usr/local/bin/node")), &apps)
                .application
                .is_none()
        );
        assert!(attribute(&raw(6, None), &apps).application.is_none());
        let mut other_user = raw(7, Some("/Applications/Test.app/Contents/Helper"));
        other_user.uid = Some(502);
        assert!(attribute(&other_user, &apps).application.is_none());
    }
    #[test]
    fn nested_helper_app_joins_its_regular_main_app_instead_of_leading_a_group() {
        let outer = "/Applications/Outer.app";
        let inner = "/Applications/Outer.app/Contents/Frameworks/Outer Helper.app";
        let processes = vec![
            raw(1, Some("/Applications/Outer.app/Contents/MacOS/Test")),
            // AppKit also lists the nested helper as a running application.
            raw(2, Some(&format!("{inner}/Contents/MacOS/Test"))),
            raw(3, Some(&format!("{inner}/Contents/MacOS/Outer Helper"))),
            // A standalone menu bar app has no regular main app to join.
            raw(4, Some("/Applications/Menu.app/Contents/MacOS/Test")),
        ];
        let listed = vec![
            app(1, outer),
            accessory(2, inner),
            accessory(4, "/Applications/Menu.app"),
        ];
        // Unfiltered, the helper bundle competes with its enclosing app.
        assert!(attribute(&processes[2], &listed).application.is_none());

        let apps = main_applications(listed, &processes);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app.leader_pid, 1);
        let infos: Vec<_> = processes
            .into_iter()
            .map(|p| {
                let attribution = attribute(&p, &apps);
                info(p, attribution)
            })
            .collect();
        for helper in &infos[1..3] {
            let application = helper.attribution.application.as_ref().unwrap();
            assert_eq!(application.leader_pid, 1);
            assert_eq!(helper.attribution.method, "same_bundle_executable");
        }
        assert!(infos[3].attribution.application.is_none());
        let groups = occupancy_groups(&infos);
        let main = groups
            .iter()
            .find(|g| g.id == format!("app:{}", infos[0].id))
            .unwrap();
        assert_eq!(main.process_ids.len(), 3);
        assert_eq!(main.memory_bytes.value, Some(30));
        assert_eq!(groups.len(), 2);
    }
    #[test]
    fn ambiguous_same_installation_and_reused_leader_do_not_attribute() {
        let apps = vec![
            app(1, "/Applications/Test.app"),
            app(2, "/Applications/Test.app"),
        ];
        assert!(
            attribute(
                &raw(3, Some("/Applications/Test.app/Contents/Helper")),
                &apps
            )
            .application
            .is_none()
        );
        let mut replacement = raw(1, Some("/Applications/Test.app/Contents/MacOS/Test"));
        replacement.identity.start_microseconds = Some(999);
        assert!(attribute(&replacement, &apps[..1]).application.is_none());
    }
    #[test]
    fn grouping_deduplicates_and_keeps_missing_unknown() {
        let apps = vec![app(1, "/Applications/Test.app")];
        let leader = raw(1, Some("/Applications/Test.app/Contents/MacOS/Test"));
        let leader = info(
            raw(1, leader.executable_path.as_deref()),
            attribute(&leader, &apps),
        );
        let helper = raw(2, Some("/Applications/Test.app/Contents/Helper"));
        let helper = info(
            raw(2, helper.executable_path.as_deref()),
            attribute(&helper, &apps),
        );
        let mut processes = vec![leader.clone(), helper, leader];
        let groups = occupancy_groups(&processes);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].memory_bytes.value, Some(20));
        assert_eq!(groups[0].process_ids.len(), 2);
        processes[1].memory_bytes = Metric::unavailable(Validity::Denied, "denied");
        let groups = occupancy_groups(&processes);
        assert_eq!(groups[0].memory_bytes.value, None);
        assert_eq!(groups[0].memory_bytes.status, Validity::Unknown);
    }
    #[test]
    fn single_process_groups_preserve_the_specific_missing_cause() {
        for status in [
            Validity::Denied,
            Validity::Exited,
            Validity::Stale,
            Validity::Unsupported,
        ] {
            let raw = raw(42, None);
            let mut process = info(
                raw,
                Attribution {
                    application: None,
                    method: "unattributed".into(),
                    confidence: "unknown".into(),
                    explanation: "no evidence".into(),
                },
            );
            process.memory_bytes = Metric::unavailable(status, "specific OS error");
            let groups = occupancy_groups(&[process]);
            assert_eq!(groups[0].memory_bytes.status, status);
            assert_eq!(groups[0].memory_bytes.value, None);
            assert_eq!(
                groups[0].memory_bytes.reason.as_deref(),
                Some("specific OS error")
            );
        }
    }
    #[test]
    fn ordinary_system_apps_are_apps_but_keep_system_protection() {
        let apps = vec![app(42, "/System/Applications/Mail.app")];
        let mail = raw(
            42,
            Some("/System/Applications/Mail.app/Contents/MacOS/Test"),
        );
        let confirmed = attribute(&mail, &apps);
        assert!(confirmed.application.is_some());
        assert_eq!(
            classify_process(&mail, &confirmed),
            (Category::Application, true)
        );

        let helper = raw(
            43,
            Some("/System/Applications/Mail.app/Contents/Frameworks/Helper"),
        );
        assert_eq!(
            classify_process(&helper, &attribute(&helper, &apps)),
            (Category::Application, true)
        );
        assert_eq!(
            classify_process(&mail, &attribute(&mail, &[])),
            (Category::System, true),
            "the directory alone cannot establish an ordinary application"
        );

        let apps = vec![app(44, "/System/Library/CoreServices/Finder.app")];
        let finder = raw(
            44,
            Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Test"),
        );
        assert_eq!(
            classify_process(&finder, &attribute(&finder, &apps)),
            (Category::System, true)
        );
        let apps = vec![app(45, "/System/ApplicationsElse/Test.app")];
        let lookalike = raw(
            45,
            Some("/System/ApplicationsElse/Test.app/Contents/MacOS/Test"),
        );
        assert_eq!(
            classify_process(&lookalike, &attribute(&lookalike, &apps)),
            (Category::System, true)
        );
    }
    #[test]
    fn cpu_requires_real_time_and_does_not_limit_multicore_to_one_hundred() {
        let now = Instant::now();
        assert!(cpu_between(None, 2_000_000_000, now).value.is_none());
        assert_eq!(
            cpu_between(Some((0, now)), 2_000_000_000, now + Duration::from_secs(1)).value,
            Some(200.0)
        );
        assert!(
            cpu_between(Some((10, now)), 9, now + Duration::from_secs(1))
                .value
                .is_none()
        );
        assert!(cpu_between(Some((0, now)), 1, now).value.is_none());
        assert!(
            cpu_between(Some((0, now)), 1, now + Duration::from_secs(30))
                .value
                .is_none()
        );
    }
}
