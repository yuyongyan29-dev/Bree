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
const CPU_SOURCE: &str = "libproc PROC_PIDTASKINFO Mach CPU ticks converted by mach_timebase_info / monotonic elapsed time";

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
        let ancestors = hosting_ancestors(&raw_processes, std::process::id());
        let hosting_bundles: HashSet<String> = raw_processes
            .iter()
            .filter(|p| ancestors.contains(&p.identity.pid))
            .filter_map(|p| attribute(p, &apps).application.map(|a| a.bundle_path))
            .collect();
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
                        CPU_SOURCE,
                        "Identity or cumulative CPU time is unreadable",
                    ),
                };
                let (category, system_object) = classify_process(&p, &attribution);
                let mut protection_reasons = vec!["Read-only Alpha: termination capability is not enabled".into()];
                if p.identity.pid == std::process::id() {
                    protection_reasons.push("Bree itself".into());
                }
                if ancestors.contains(&p.identity.pid)
                    || attribution
                        .application
                        .as_ref()
                        .is_some_and(|a| hosting_bundles.contains(&a.bundle_path))
                {
                    protection_reasons.push("The terminal or session hosting this Bree instance".into());
                }
                if p.uid.is_some_and(|uid| uid != self.platform.current_uid()) {
                    protection_reasons.push("Another user or a system account".into());
                }
                if p.uid.is_none() {
                    protection_reasons.push("User identity cannot be read reliably".into());
                }
                if system_object {
                    protection_reasons.push("System object".into());
                }
                if p.identity.status != Validity::Ok {
                    protection_reasons.push("Instance identity cannot be read reliably".into());
                }
                if attribution
                    .application
                    .as_ref()
                    .is_some_and(|a| a.frontmost)
                {
                    protection_reasons.push("In the foreground at sampling time (observation only, not an execution basis)".into());
                }
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
                    protection_reasons,
                    quit_supported: false,
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
            CPU_SOURCE,
            "The first sample establishes a baseline; the next sample needs a real time interval",
        );
    };
    let Some(delta_cpu) = total_ns.checked_sub(previous_total) else {
        return Metric::unavailable(
            Validity::Stale,
            CPU_SOURCE,
            "Cumulative CPU time decreased; rebuilding the baseline",
        );
    };
    let elapsed = now.saturating_duration_since(previous_time);
    if elapsed.as_secs() >= 30 {
        return Metric::unavailable(
            Validity::Unknown,
            CPU_SOURCE,
            "Sampling interval reached 30 seconds; rebuilding the CPU baseline after wake or prolonged inactivity",
        );
    }
    let elapsed_ns = elapsed.as_nanos();
    if elapsed_ns == 0 {
        return Metric::unavailable(Validity::Unknown, CPU_SOURCE, "Sampling interval is zero");
    }
    Metric::ok(delta_cpu as f64 / elapsed_ns as f64 * 100.0, CPU_SOURCE)
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

fn hosting_ancestors(processes: &[RawProcess], own_pid: u32) -> HashSet<u32> {
    let parents: HashMap<u32, Option<u32>> = processes
        .iter()
        .map(|p| (p.identity.pid, p.parent_pid))
        .collect();
    let mut result = HashSet::new();
    let mut current = own_pid;
    while result.insert(current) {
        match parents.get(&current).copied().flatten() {
            Some(parent) if parent > 0 && parent != current => current = parent,
            _ => break,
        }
    }
    result
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

fn occupancy_groups(processes: &[ProcessInfo]) -> Vec<OccupancyGroup> {
    let mut groups: BTreeMap<String, OccupancyGroup> = BTreeMap::new();
    let mut seen = HashSet::new();
    for process in processes {
        if !seen.insert(process.id.clone()) {
            continue;
        }
        let (id, name, explanation) = if let Some(app) = &process.attribution.application {
            let leader = processes.iter().find(|p| p.identity.pid == app.leader_pid);
            let anchor = leader.map(|p| p.id.as_str()).unwrap_or(process.id.as_str());
            (
                format!("app:{anchor}"),
                app.name.clone(),
                "RSS total of the same running main app and processes in its bundle; each instance is counted once, and missing values are not replaced with zero.".into(),
            )
        } else {
            (
                process.id.clone(),
                process.name.clone(),
                process.attribution.explanation.clone(),
            )
        };
        let group = groups.entry(id.clone()).or_insert_with(|| OccupancyGroup {
            id,
            name,
            category: process.category,
            memory_bytes: process.memory_bytes.clone(),
            metric_kind: PROCESS_METRIC_KIND.into(),
            process_ids: Vec::new(),
            explanation,
        });
        let first_member = group.process_ids.is_empty();
        group.process_ids.push(process.id.clone());
        if first_member {
            // A one-process group is that exact metric, including its denial,
            // exit, unsupported status, source, and reason. Only an incomplete
            // aggregate requires an aggregate-level unknown value.
            continue;
        }
        match (group.memory_bytes.value, process.memory_bytes.value) {
            (Some(sum), Some(value)) if process.memory_bytes.status == Validity::Ok => {
                match sum.checked_add(value) {
                    Some(total) => {
                        group.memory_bytes = Metric::ok(total, "sum of distinct process RSS")
                    }
                    None => {
                        group.memory_bytes = Metric::unavailable(
                            Validity::Unknown,
                            "sum of distinct process RSS",
                            "Group total overflow",
                        )
                    }
                }
            }
            _ => {
                group.memory_bytes = Metric::unavailable(
                    Validity::Unknown,
                    "sum of distinct process RSS",
                    "At least one instance in the group has missing memory; the total is unknown, see individual processes",
                )
            }
        }
    }
    let mut groups: Vec<_> = groups.into_values().collect();
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
            memory_bytes: Metric::ok(10, "test"),
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
            cpu_one_core_percent: Metric::unavailable(Validity::Unknown, "test", "baseline"),
            category: Category::Application,
            attribution,
            protection_reasons: vec![],
            quit_supported: false,
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
        processes[1].memory_bytes = Metric::unavailable(Validity::Denied, "test", "denied");
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
            process.memory_bytes =
                Metric::unavailable(status, "fixture RSS source", "specific OS error");
            let groups = occupancy_groups(&[process]);
            assert_eq!(groups[0].memory_bytes.status, status);
            assert_eq!(groups[0].memory_bytes.value, None);
            assert_eq!(groups[0].memory_bytes.source, "fixture RSS source");
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
    #[test]
    fn hosting_ancestry_stops_at_cycles_and_does_not_attribute_children() {
        let mut a = raw(10, None);
        a.parent_pid = Some(11);
        let mut b = raw(11, None);
        b.parent_pid = Some(10);
        let mut child = raw(12, None);
        child.parent_pid = Some(10);
        assert_eq!(
            hosting_ancestors(&[a, b, child], 10),
            HashSet::from([10, 11])
        );
    }
}
