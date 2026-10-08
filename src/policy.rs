//! Deterministic classification and exact installation rules. No process actions.
use crate::model::{ActivationPolicy, Category, ProcessIdentity, ProcessInfo, Snapshot, Validity};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path};

pub const POLICY_VERSION: u32 = 1;
const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Protect,
}

/// A rule selects one installation, never a name, PID, wildcard, or child tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppScope {
    pub bundle_id: String,
    pub bundle_path: String,
    pub executable_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    pub id: String,
    pub action: RuleAction,
    pub scope: AppScope,
    pub source: String,
    pub enabled: bool,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyState {
    pub schema_version: u32,
    pub revision: u64,
    pub rules: Vec<PolicyRule>,
}

impl Default for PolicyState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            revision: 0,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleChange {
    Add { action: RuleAction, scope: AppScope },
    Remove { id: String },
}

impl PolicyState {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != STATE_SCHEMA_VERSION {
            return Err("Unsupported rule file schema_version; remaining read-only.".into());
        }
        let mut ids = HashSet::new();
        for rule in &self.rules {
            if rule.id.is_empty()
                || rule.id.len() > 128
                || !rule
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b))
            {
                return Err("Invalid rule ID.".into());
            }
            if !ids.insert(&rule.id) {
                return Err("Duplicate rule ID.".into());
            }
            if rule.source != "user" {
                return Err(
                    "Rules currently accept only the user source explicitly set by the user."
                        .into(),
                );
            }
            rule.scope.validate()?;
        }
        Ok(())
    }

    pub fn apply(&self, change: RuleChange, now: u64) -> Result<Self, String> {
        self.validate()?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("Rule revision overflow; writing stopped.")?;
        let mut next = self.clone();
        match change {
            RuleChange::Add { action, scope } => {
                scope.validate()?;
                if next
                    .rules
                    .iter()
                    .any(|r| r.action == action && r.scope == scope && r.enabled)
                {
                    return Err(
                        "An identical enabled rule already exists for this installation scope."
                            .into(),
                    );
                }
                let id = format!("rule:{revision}:{now}");
                if next.rules.iter().any(|r| r.id == id) {
                    return Err(
                        "The new rule ID conflicts with an existing rule; writing stopped.".into(),
                    );
                }
                next.rules.push(PolicyRule {
                    id,
                    action,
                    scope,
                    source: "user".into(),
                    enabled: true,
                    created_at_unix_ms: now,
                });
            }
            RuleChange::Remove { id } => {
                let index = next
                    .rules
                    .iter()
                    .position(|rule| rule.id == id)
                    .ok_or("The rule to remove does not exist; reload the rules.")?;
                next.rules.remove(index);
            }
        }
        next.revision = revision;
        next.validate()?;
        Ok(next)
    }
}

impl AppScope {
    pub fn validate(&self) -> Result<(), String> {
        if !self.bundle_id.contains('.')
            || self.bundle_id.split('.').any(str::is_empty)
            || !self
                .bundle_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(
                "The bundle ID must be exact and contain no wildcards or control characters."
                    .into(),
            );
        }
        if !exact_absolute_path(&self.bundle_path)
            || !self.bundle_path.ends_with(".app")
            || !exact_absolute_path(&self.executable_path)
        {
            return Err("Rules require normalized absolute .app and main executable paths; wildcards and relative paths are not accepted.".into());
        }
        let macos = Path::new(&self.bundle_path).join("Contents/MacOS");
        if Path::new(&self.executable_path).parent() != Some(macos.as_path()) {
            return Err(
                "The main executable must be directly inside Contents/MacOS of this installation."
                    .into(),
            );
        }
        Ok(())
    }
}

fn exact_absolute_path(value: &str) -> bool {
    !value.is_empty()
        && !value.ends_with('/')
        && !value.chars().any(|c| {
            c.is_control()
                || matches!(c, '*' | '?' | '[' | ']' | '\\' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        && Path::new(value).is_absolute()
        && value
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

pub(crate) fn reliable_identity(process: &ProcessInfo) -> bool {
    process.identity.status == Validity::Ok
        && process.identity.pid > 1
        && !process.identity.boot_session.is_empty()
        && !process.identity.boot_session.chars().any(char::is_control)
        && process
            .identity
            .start_seconds
            .is_some_and(|seconds| seconds > 0)
        && process
            .identity
            .start_microseconds
            .is_some_and(|micros| micros < 1_000_000)
        && process.id == process.identity.object_id()
}

/// Reconstruct the exact current leader from the group. No filesystem/name guessing.
fn scoped_leader<'a>(
    snapshot: &'a Snapshot,
    group_id: &str,
) -> Result<(AppScope, &'a ProcessInfo), String> {
    let mut groups = snapshot.groups.iter().filter(|group| group.id == group_id);
    let group = groups
        .next()
        .ok_or("The object is not in the current sample; analyze again.")?;
    if groups.next().is_some() || group.process_ids.is_empty() {
        return Err("The group ID conflicts or the group has no instances.".into());
    }
    if group.category != Category::Application {
        return Err("Only reliably identified ordinary main apps can have A1 installation rules; this object remains read-only.".into());
    }
    let mut members = Vec::with_capacity(group.process_ids.len());
    let mut ids = HashSet::new();
    for id in &group.process_ids {
        if !ids.insert(id) {
            return Err("Duplicate instances in the group.".into());
        }
        let mut matches = snapshot
            .processes
            .iter()
            .filter(|process| process.id == *id);
        let member = matches
            .next()
            .ok_or("An instance in the group is missing.")?;
        if matches.next().is_some() || !reliable_identity(member) {
            return Err(
                "An instance identity in the group is missing, stale, or conflicting.".into(),
            );
        }
        members.push(member);
    }
    let mut leaders = members.iter().copied().filter(|process| {
        process.attribution.method == "appkit_main_application"
            && process.attribution.confidence == "high"
            && process
                .attribution
                .application
                .as_ref()
                .is_some_and(|app| app.leader_pid == process.identity.pid)
    });
    let leader = leaders.next().ok_or("No verified AppKit main app.")?;
    if leaders.next().is_some()
        || snapshot
            .processes
            .iter()
            .filter(|process| process.identity.pid == leader.identity.pid)
            .count()
            != 1
        || group.id != format!("app:{}", leader.id)
    {
        return Err("The main app or current group instance identity conflicts.".into());
    }
    let app = leader.attribution.application.as_ref().unwrap();
    if app.activation_policy != ActivationPolicy::Regular {
        return Err("The AppKit app is not an ordinary Regular app; helper, menu bar and background apps remain read-only.".into());
    }
    let scope = AppScope {
        bundle_id: app
            .bundle_id
            .clone()
            .ok_or("The main app bundle ID is unreadable.")?,
        bundle_path: app.bundle_path.clone(),
        executable_path: leader
            .executable_path
            .clone()
            .ok_or("The main app executable path is unreadable.")?,
    };
    scope.validate()?;
    let contents = Path::new(&scope.bundle_path).join("Contents");
    for member in members {
        if crate::attribution::label_process(member).is_some() {
            return Err(
                "Contains a recognized AI / development tool; task completion, sharing, and the control contract are unverified, so it remains read-only.".into(),
            );
        }
        let member_app = member
            .attribution
            .application
            .as_ref()
            .ok_or("Attribution is missing within the group.")?;
        if member.category != Category::Application
            || member_app.leader_pid != app.leader_pid
            || member_app.bundle_id != app.bundle_id
            || member_app.bundle_path != app.bundle_path
            || member_app.frontmost != app.frontmost
            || member.uid != leader.uid
            || !member.executable_path.as_deref().is_some_and(|path| {
                exact_absolute_path(path) && Path::new(path).starts_with(&contents)
            })
            || (member.id != leader.id
                && (member.attribution.method != "same_bundle_executable"
                    || member.attribution.confidence != "installation_evidence"))
        {
            return Err("Installation, user, or attribution evidence conflicts within the group; scope is not expanded to helpers or replacement instances.".into());
        }
    }
    Ok((scope, leader))
}

pub fn scope_for_group(snapshot: &Snapshot, group_id: &str) -> Result<AppScope, String> {
    scoped_leader(snapshot, group_id).map(|(scope, _)| scope)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyContext {
    pub state_valid: bool,
    pub a1_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Automatic,
    Pending,
    Protected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub group_id: String,
    pub name: String,
    pub disposition: Disposition,
    pub reasons: Vec<String>,
    pub matched_rule_ids: Vec<String>,
    pub target_identity: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub schema_version: u32,
    pub policy_version: u32,
    pub plan_id: String,
    pub created_at_unix_ms: u64,
    pub rule_revision: u64,
    pub read_only: bool,
    pub entries: Vec<PlanEntry>,
    pub automatic_count: usize,
    pub pending_count: usize,
    pub protected_count: usize,
}

pub fn evaluate(snapshot: &Snapshot, state: &PolicyState, context: &PolicyContext) -> CleanupPlan {
    let validation = state.validate();
    let valid_state = context.state_valid && validation.is_ok();
    let entries: Vec<_> = snapshot
        .groups
        .iter()
        .map(|group| {
            let mut entry = PlanEntry {
                group_id: group.id.clone(),
                name: group.name.clone(),
                disposition: Disposition::Protected,
                reasons: Vec::new(),
                matched_rule_ids: Vec::new(),
                target_identity: None,
            };
            if !valid_state {
                entry.reasons.push(match &validation {
                    Err(reason) => format!("Invalid rule file: {reason}"),
                    Ok(()) => "Rules were not loaded reliably; remaining read-only with no executable candidates.".into(),
                });
            }
            let (scope, leader) = match scoped_leader(snapshot, &group.id) {
                Ok(value) => value,
                Err(reason) => {
                    entry.reasons.push(reason);
                    return entry;
                }
            };
            entry.target_identity = Some(leader.identity.clone());
            let rules: Vec<_> = if valid_state {
                state
                    .rules
                    .iter()
                    .filter(|rule| rule.enabled && rule.scope == scope)
                    .collect()
            } else {
                Vec::new()
            };
            entry.matched_rule_ids = rules.iter().map(|rule| rule.id.clone()).collect();
            let allowed = rules.iter().any(|rule| rule.action == RuleAction::Allow);
            if rules.iter().any(|rule| rule.action == RuleAction::Protect) {
                entry
                    .reasons
                    .push("A user protection rule matches; protection always overrides allow.".into());
            }
            if leader.uid.is_none() || leader.uid != Some(unsafe { libc::geteuid() }) {
                entry.reasons.push("The current user identity does not match or is unreadable.".into());
            }
            if leader
                .attribution
                .application
                .as_ref()
                .is_some_and(|app| app.frontmost)
            {
                entry
                    .reasons
                    .push("The main app is in the foreground in the current sample; skipped for protection.".into());
            }
            for member in snapshot
                .processes
                .iter()
                .filter(|process| group.process_ids.contains(&process.id))
            {
                if member.identity.pid == std::process::id() {
                    push_unique(&mut entry.reasons, "Bree itself".into());
                }
                if member.identity.pid <= 1
                    || member.uid == Some(0)
                    || member.executable_path.as_deref().is_some_and(|path| {
                        ["/System", "/usr/libexec", "/usr/sbin", "/sbin"]
                            .iter()
                            .any(|root| Path::new(path).starts_with(root))
                    })
                {
                    push_unique(&mut entry.reasons, "System object".into());
                }
                for reason in &member.protection_reasons {
                    // This sole presentation reason is represented by explicit capability gates.
                    // Unknown/new protection reasons always fail closed.
                    if reason != "Read-only Alpha: termination capability is not enabled" {
                        push_unique(&mut entry.reasons, reason.clone());
                    }
                }
            }
            if !entry.reasons.is_empty() {
                return entry;
            }
            if !allowed {
                entry.disposition = Disposition::Pending;
                entry
                    .reasons
                    .push("The current ordinary main app has reliable installation and instance evidence, but no user allow rule.".into());
                entry
                    .reasons
                    .push("Needs review only invites explicit installation rules; it does not mean the app is unnecessary or may be quit.".into());
                if !context.a1_enabled || !leader.quit_supported {
                    entry
                        .reasons
                        .push("Normal termination is not enabled; this is a dry run, and no termination requests are sent.".into());
                }
            } else if !context.a1_enabled || !leader.quit_supported {
                entry
                    .reasons
                    .push("A user allow rule matches, but normal termination is unverified or disabled; skipped for protection.".into());
            } else {
                entry.disposition = Disposition::Automatic;
                entry.reasons.push(
                    "An exact installation allow rule matches; the current main app instance is valid and in the background, no protection conflicts exist, and A1 is enabled."
                        .into(),
                );
                entry
                    .reasons
                    .push("This dry run only freezes the main app instance; it does not include child processes in the group and sends no requests.".into());
            }
            entry
        })
        .collect();
    let mut fingerprint = 0xcbf29ce484222325_u64;
    for entry in &entries {
        let disposition = match entry.disposition {
            Disposition::Automatic => "automatic",
            Disposition::Pending => "pending",
            Disposition::Protected => "protected",
        };
        for part in std::iter::once(entry.group_id.as_str())
            .chain(std::iter::once(disposition))
            .chain(entry.matched_rule_ids.iter().map(String::as_str))
            .chain(entry.reasons.iter().map(String::as_str))
        {
            for byte in part.bytes().chain(std::iter::once(0xff)) {
                fingerprint = (fingerprint ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
    }
    CleanupPlan {
        schema_version: STATE_SCHEMA_VERSION,
        policy_version: POLICY_VERSION,
        plan_id: format!(
            "plan:{}:{}:{fingerprint:016x}",
            snapshot.sampled_at_unix_ms, state.revision
        ),
        created_at_unix_ms: snapshot.sampled_at_unix_ms,
        rule_revision: state.revision,
        read_only: true,
        automatic_count: entries
            .iter()
            .filter(|e| e.disposition == Disposition::Automatic)
            .count(),
        pending_count: entries
            .iter()
            .filter(|e| e.disposition == Disposition::Pending)
            .count(),
        protected_count: entries
            .iter()
            .filter(|e| e.disposition == Disposition::Protected)
            .count(),
        entries,
    }
}

fn push_unique(reasons: &mut Vec<String>, reason: String) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Application, Attribution, Coverage, Metric, OccupancyGroup, Pressure, SystemMemory,
    };

    fn application_snapshot() -> Snapshot {
        let identity = ProcessIdentity {
            boot_session: "test-boot".into(),
            pid: 42,
            start_seconds: Some(10),
            start_microseconds: Some(1),
            status: Validity::Ok,
        };
        let id = identity.object_id();
        Snapshot {
            schema_version: 1,
            sampled_at_unix_ms: 1_000,
            collected_in_ms: 1,
            system: SystemMemory {
                total_bytes: Metric::ok(100, "test"),
                used_bytes: Metric::ok(50, "test"),
                compressed_bytes: Metric::ok(0, "test"),
                swap_used_bytes: Metric::ok(0, "test"),
                cached_bytes: Metric::ok(0, "test"),
                pressure: Metric::ok(Pressure::Normal, "test"),
                used_definition: "test".into(),
            },
            processes: vec![ProcessInfo {
                id: id.clone(),
                identity,
                parent_pid: None,
                uid: Some(unsafe { libc::geteuid() }),
                name: "Example".into(),
                executable_path: Some("/Applications/Example.app/Contents/MacOS/Example".into()),
                memory_bytes: Metric::ok(10, "test"),
                metric_kind: "rss".into(),
                cpu_one_core_percent: Metric::ok(0.0, "test"),
                category: Category::Application,
                attribution: Attribution {
                    application: Some(Application {
                        bundle_id: Some("com.example.App".into()),
                        bundle_path: "/Applications/Example.app".into(),
                        name: "Example".into(),
                        leader_pid: 42,
                        frontmost: false,
                        activation_policy: ActivationPolicy::Regular,
                    }),
                    method: "appkit_main_application".into(),
                    confidence: "high".into(),
                    explanation: "test".into(),
                },
                protection_reasons: vec![
                    "Read-only Alpha: termination capability is not enabled".into(),
                ],
                quit_supported: false,
            }],
            groups: vec![OccupancyGroup {
                id: format!("app:{id}"),
                name: "Example".into(),
                category: Category::Application,
                memory_bytes: Metric::ok(10, "test"),
                metric_kind: "rss".into(),
                process_ids: vec![id],
                explanation: "test".into(),
            }],
            coverage: Coverage {
                enumerated_processes: 1,
                readable_memory_processes: 1,
                reliable_identity_processes: 1,
                notes: vec![],
            },
            diagnostics: vec![],
        }
    }

    fn valid_context() -> PolicyContext {
        PolicyContext {
            state_valid: true,
            a1_enabled: false,
        }
    }

    fn add(state: &PolicyState, action: RuleAction, scope: AppScope) -> PolicyState {
        state
            .apply(RuleChange::Add { action, scope }, 1_000 + state.revision)
            .unwrap()
    }

    fn allowed(snapshot: &Snapshot) -> PolicyState {
        add(
            &PolicyState::default(),
            RuleAction::Allow,
            scope_for_group(snapshot, &snapshot.groups[0].id).unwrap(),
        )
    }

    #[test]
    fn zero_rules_means_zero_automatic_and_explained_pending() {
        let snapshot = application_snapshot();
        let plan = evaluate(&snapshot, &PolicyState::default(), &valid_context());
        assert_eq!(plan.automatic_count, 0);
        assert_eq!(plan.pending_count, 1);
        assert!(plan.read_only);
        assert!(
            plan.entries[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("no termination requests are sent"))
        );
        assert_eq!(
            plan.entries[0].target_identity,
            Some(snapshot.processes[0].identity.clone())
        );
    }

    #[test]
    fn non_regular_apps_get_no_scope_and_stay_protected_even_with_allow() {
        let regular = application_snapshot();
        let state = allowed(&regular);
        for policy in [
            ActivationPolicy::Accessory,
            ActivationPolicy::Prohibited,
            ActivationPolicy::Unknown,
        ] {
            let mut snapshot = regular.clone();
            snapshot.processes[0]
                .attribution
                .application
                .as_mut()
                .unwrap()
                .activation_policy = policy;
            snapshot.processes[0].quit_supported = true;
            assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
            let plan = evaluate(
                &snapshot,
                &state,
                &PolicyContext {
                    state_valid: true,
                    a1_enabled: true,
                },
            );
            assert_eq!(plan.protected_count, 1, "{policy:?}");
            assert_eq!(plan.pending_count, 0);
            assert!(plan.entries[0].matched_rule_ids.is_empty());
            assert!(plan.entries[0].reasons[0].contains("not an ordinary Regular app"));
        }
    }

    #[test]
    fn allow_cannot_bypass_capability_and_protect_overrides_allow() {
        let mut snapshot = application_snapshot();
        let state = allowed(&snapshot);
        assert_eq!(
            evaluate(&snapshot, &state, &valid_context()).protected_count,
            1
        );
        let enabled = PolicyContext {
            state_valid: true,
            a1_enabled: true,
        };
        assert_eq!(evaluate(&snapshot, &state, &enabled).automatic_count, 0);
        snapshot.processes[0].quit_supported = true;
        assert_eq!(evaluate(&snapshot, &state, &enabled).automatic_count, 1);
        let protected = add(&state, RuleAction::Protect, state.rules[0].scope.clone());
        let plan = evaluate(&snapshot, &protected, &enabled);
        assert_eq!(plan.protected_count, 1);
        assert_eq!(plan.entries[0].matched_rule_ids.len(), 2);
        assert!(
            plan.entries[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("protection always overrides allow"))
        );
    }

    #[test]
    fn frontend_change_and_hard_protection_reclassify_allowed_leader() {
        let mut snapshot = application_snapshot();
        let state = allowed(&snapshot);
        let enabled = PolicyContext {
            state_valid: true,
            a1_enabled: true,
        };
        snapshot.processes[0].quit_supported = true;
        assert_eq!(evaluate(&snapshot, &state, &enabled).automatic_count, 1);
        snapshot.processes[0]
            .attribution
            .application
            .as_mut()
            .unwrap()
            .frontmost = true;
        assert_eq!(evaluate(&snapshot, &state, &enabled).protected_count, 1);
        snapshot.processes[0]
            .attribution
            .application
            .as_mut()
            .unwrap()
            .frontmost = false;
        snapshot.processes[0]
            .protection_reasons
            .push("The terminal or session hosting this Bree instance".into());
        assert_eq!(evaluate(&snapshot, &state, &enabled).automatic_count, 0);
    }

    #[test]
    fn same_name_different_installation_does_not_match_rule() {
        let mut snapshot = application_snapshot();
        let state = allowed(&snapshot);
        snapshot.processes[0]
            .attribution
            .application
            .as_mut()
            .unwrap()
            .bundle_path = "/Users/example/Example.app".into();
        snapshot.processes[0].executable_path =
            Some("/Users/example/Example.app/Contents/MacOS/Example".into());
        snapshot.processes[0].quit_supported = true;
        let plan = evaluate(
            &snapshot,
            &state,
            &PolicyContext {
                state_valid: true,
                a1_enabled: true,
            },
        );
        assert_eq!(plan.pending_count, 1);
        assert!(plan.entries[0].matched_rule_ids.is_empty());
    }

    #[test]
    fn stale_group_or_process_identity_never_targets_reused_pid() {
        let mut snapshot = application_snapshot();
        let old_identity = snapshot.processes[0].identity.clone();
        let state = allowed(&snapshot);
        snapshot.processes[0].identity.start_microseconds = Some(2);
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
        let plan = evaluate(&snapshot, &state, &valid_context());
        assert_eq!(plan.protected_count, 1);
        assert!(plan.entries[0].target_identity.is_none());
        let replacement = snapshot.processes[0].identity.object_id();
        snapshot.processes[0].id = replacement.clone();
        snapshot.groups[0].process_ids = vec![replacement.clone()];
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
        snapshot.groups[0].id = format!("app:{replacement}");
        let plan = evaluate(&snapshot, &state, &valid_context());
        assert_eq!(
            plan.entries[0]
                .target_identity
                .as_ref()
                .unwrap()
                .start_microseconds,
            Some(2)
        );
        assert_ne!(
            plan.entries[0].target_identity.as_ref().unwrap(),
            &old_identity
        );
    }

    #[test]
    fn malformed_rules_and_failed_state_loading_fail_closed() {
        let snapshot = application_snapshot();
        let state = allowed(&snapshot);
        let failed_load = PolicyContext {
            state_valid: false,
            a1_enabled: true,
        };
        assert_eq!(evaluate(&snapshot, &state, &failed_load).protected_count, 1);
        let mut invalid = state.clone();
        invalid.rules[0].scope.bundle_path = "/Applications/*.app".into();
        assert!(invalid.validate().is_err());
        let plan = evaluate(&snapshot, &invalid, &valid_context());
        assert_eq!(plan.automatic_count, 0);
        assert_eq!(plan.protected_count, 1);
        assert!(plan.entries[0].matched_rule_ids.is_empty());
        assert!(
            plan.entries[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("Invalid rule file"))
        );
    }

    #[test]
    fn only_leader_is_frozen_and_helper_conflicts_are_rejected() {
        let mut snapshot = application_snapshot();
        let state = allowed(&snapshot);
        let mut helper = snapshot.processes[0].clone();
        helper.identity.pid = 43;
        helper.id = helper.identity.object_id();
        helper.executable_path =
            Some("/Applications/Example.app/Contents/Frameworks/Helper".into());
        helper.attribution.method = "same_bundle_executable".into();
        helper.attribution.confidence = "installation_evidence".into();
        snapshot.groups[0].process_ids.push(helper.id.clone());
        snapshot.processes.push(helper);
        snapshot.processes[0].quit_supported = true;
        let context = PolicyContext {
            state_valid: true,
            a1_enabled: true,
        };
        let plan = evaluate(&snapshot, &state, &context);
        assert_eq!(plan.automatic_count, 1);
        assert_eq!(plan.entries[0].target_identity.as_ref().unwrap().pid, 42);
        assert_eq!(snapshot.groups[0].process_ids.len(), 2);
        snapshot.processes[1]
            .attribution
            .application
            .as_mut()
            .unwrap()
            .leader_pid = 999;
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
        assert_eq!(evaluate(&snapshot, &state, &context).protected_count, 1);
    }

    #[test]
    fn missing_identity_and_unknown_ai_services_keep_read_only() {
        let mut snapshot = application_snapshot();
        snapshot.processes[0].identity.start_microseconds = None;
        snapshot.processes[0].id = snapshot.processes[0].identity.object_id();
        snapshot.groups[0].process_ids[0] = snapshot.processes[0].id.clone();
        snapshot.groups[0].id = format!("app:{}", snapshot.processes[0].id);
        assert_eq!(
            evaluate(&snapshot, &PolicyState::default(), &valid_context()).protected_count,
            1
        );
        for category in [Category::Unknown, Category::System] {
            let mut snapshot = application_snapshot();
            snapshot.groups[0].category = category;
            assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
            assert_eq!(
                evaluate(&snapshot, &PolicyState::default(), &valid_context()).protected_count,
                1
            );
        }
    }

    #[test]
    fn changes_preserve_unrelated_rules_and_undo_is_specific() {
        let snapshot = application_snapshot();
        let allow = allowed(&snapshot);
        let protect = add(&allow, RuleAction::Protect, allow.rules[0].scope.clone());
        assert_eq!(protect.revision, 2);
        assert_eq!(protect.rules[0], allow.rules[0]);
        let undone = protect
            .apply(
                RuleChange::Remove {
                    id: protect.rules[1].id.clone(),
                },
                1_002,
            )
            .unwrap();
        assert_eq!(undone.revision, 3);
        assert_eq!(undone.rules, allow.rules);
        assert!(
            undone
                .apply(
                    RuleChange::Remove {
                        id: "missing".into()
                    },
                    1_003
                )
                .is_err()
        );
        assert!(
            undone
                .apply(
                    RuleChange::Add {
                        action: RuleAction::Allow,
                        scope: allow.rules[0].scope.clone()
                    },
                    1_003
                )
                .is_err()
        );
    }

    #[test]
    fn disabled_protection_does_not_match_and_unknown_protection_always_blocks() {
        let mut snapshot = application_snapshot();
        let allow = allowed(&snapshot);
        let mut state = add(&allow, RuleAction::Protect, allow.rules[0].scope.clone());
        state.rules[1].enabled = false;
        snapshot.processes[0].quit_supported = true;
        let context = PolicyContext {
            state_valid: true,
            a1_enabled: true,
        };
        assert_eq!(evaluate(&snapshot, &state, &context).automatic_count, 1);
        snapshot.processes[0]
            .protection_reasons
            .push("Future protection condition".into());
        assert_eq!(evaluate(&snapshot, &state, &context).protected_count, 1);
    }

    #[test]
    fn exact_scope_rejects_wildcard_relative_and_helper_paths() {
        let snapshot = application_snapshot();
        let valid = scope_for_group(&snapshot, &snapshot.groups[0].id).unwrap();
        for path in [
            "Applications/Example.app",
            "/Applications/../Example.app",
            "/Applications//Example.app",
            "/Applications/Ex*.app",
            "/Applications/Example.app/",
        ] {
            let mut scope = valid.clone();
            scope.bundle_path = path.into();
            assert!(scope.validate().is_err(), "{path}");
        }
        let mut helper = valid.clone();
        helper.executable_path = "/Applications/Example.app/Contents/Frameworks/Helper".into();
        assert!(helper.validate().is_err());
        let mut wildcard_id = valid;
        wildcard_id.bundle_id = "com.*".into();
        assert!(wildcard_id.validate().is_err());
    }

    #[test]
    fn identified_development_tool_is_read_only_even_with_appkit_evidence() {
        let mut snapshot = application_snapshot();
        let app = snapshot.processes[0]
            .attribution
            .application
            .as_mut()
            .unwrap();
        app.bundle_id = Some("com.openai.CodexCLI".into());
        app.bundle_path =
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app".into();
        snapshot.processes[0].executable_path = Some("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex".into());
        snapshot.processes[0].quit_supported = true;
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
        let plan = evaluate(
            &snapshot,
            &PolicyState::default(),
            &PolicyContext {
                state_valid: true,
                a1_enabled: true,
            },
        );
        assert_eq!(plan.protected_count, 1);
        assert!(
            plan.entries[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("control contract are unverified"))
        );
    }

    #[test]
    fn missing_user_duplicate_instances_and_bad_attribution_cannot_create_rules() {
        let mut snapshot = application_snapshot();
        snapshot.processes[0].uid = None;
        let plan = evaluate(&snapshot, &PolicyState::default(), &valid_context());
        assert_eq!(plan.protected_count, 1);
        assert!(
            plan.entries[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("user identity"))
        );
        let mut snapshot = application_snapshot();
        snapshot.processes.push(snapshot.processes[0].clone());
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
        let mut snapshot = application_snapshot();
        snapshot.processes[0].attribution.confidence = "unknown".into();
        assert!(scope_for_group(&snapshot, &snapshot.groups[0].id).is_err());
    }

    #[test]
    fn invalid_rule_source_schema_ids_and_revision_overflow_are_rejected() {
        let snapshot = application_snapshot();
        let state = allowed(&snapshot);
        let mut invalid = state.clone();
        invalid.schema_version = 2;
        assert!(invalid.validate().is_err());
        let mut invalid = state.clone();
        invalid.rules[0].source = "inferred".into();
        assert!(invalid.validate().is_err());
        let mut invalid = state.clone();
        invalid.rules.push(invalid.rules[0].clone());
        assert!(invalid.validate().is_err());
        let mut invalid = state.clone();
        invalid.rules[0].id = "rule\u{1b}[31m".into();
        assert!(invalid.validate().is_err());
        let mut overflow = state;
        overflow.revision = u64::MAX;
        assert!(
            overflow
                .apply(
                    RuleChange::Remove {
                        id: overflow.rules[0].id.clone()
                    },
                    1_000
                )
                .is_err()
        );
    }
}
