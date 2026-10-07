//! A main-thread, time-driven cleanup session. Never rebind a frozen handle by PID.
//! Production A1 remains gated until its native adapter passes capability acceptance.
use crate::{
    collect::Collector,
    model::{ProcessIdentity, Snapshot, SystemMemory},
    policy::{
        self, AppScope, CleanupPlan, Disposition, PolicyContext, PolicyRule, PolicyState,
        RuleAction,
    },
    storage::{ExecutionGuard, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const EXIT_WAIT: Duration = Duration::from_secs(15);
pub const RESOURCE_WINDOW: Duration = Duration::from_secs(10);
const PLAN_MAX_AGE: Duration = Duration::from_secs(30);

/// No environment variable or CLI flag can bypass the capability gate.
pub fn enabled() -> bool {
    false
}
pub fn capability_reason() -> &'static str {
    "普通应用的精确内核实例递送契约尚未验证，实际退出继续关闭"
}

#[derive(Debug, Clone)]
pub enum CleanupMode {
    Automatic,
    Manual { group_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Exited,
    StillRunning,
    RequestRefused,
    Skipped,
    Cancelled,
    Unknown,
}
impl Outcome {
    pub fn text(self) -> &'static str {
        match self {
            Self::Exited => "已退出",
            Self::StillRunning => "仍在运行",
            Self::RequestRefused => "请求被拒绝",
            Self::Skipped => "已跳过",
            Self::Cancelled => "未发送／已取消",
            Self::Unknown => "尚未核验",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetResult {
    pub group_id: String,
    pub name: String,
    pub identity: Option<ProcessIdentity>,
    pub outcome: Outcome,
    pub reason: String,
    pub request_sent: bool,
    pub observed_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResult {
    pub schema_version: u32,
    pub run_id: String,
    pub plan_id: String,
    pub rule_revision: u64,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    pub cancelled: bool,
    pub targets: Vec<TargetResult>,
    pub resource_before: SystemMemory,
    pub resource_after: Option<SystemMemory>,
    pub resource_observation: String,
    pub errors: Vec<String>,
}
impl CleanupResult {
    pub fn exit_code(&self) -> i32 {
        if self.cancelled {
            return 130;
        }
        if !self.errors.is_empty() {
            return 1;
        }
        if self.targets.is_empty() {
            return 0;
        }
        if self
            .targets
            .iter()
            .all(|t| !t.request_sent && t.outcome == Outcome::Skipped)
        {
            return 4;
        }
        if self.targets.iter().any(|t| t.outcome != Outcome::Exited) {
            return 3;
        }
        0
    }
}

#[derive(Debug)]
enum Observation {
    Running,
    Exited { restarted: bool },
    Unknown(String),
}
/// Every handle represents one retained native application instance, never a PID route.
trait Backend {
    fn pump(&mut self) {}
    fn available(&self) -> bool;
    fn snapshot(&mut self) -> Result<Snapshot, String>;
    fn freeze(&mut self, identity: &ProcessIdentity, scope: &AppScope) -> Result<usize, String>;
    fn request(
        &mut self,
        handle: usize,
        identity: &ProcessIdentity,
        scope: &AppScope,
        cancelled: &AtomicBool,
    ) -> Result<bool, String>;
    fn observe(
        &mut self,
        handle: usize,
        identity: &ProcessIdentity,
        scope: &AppScope,
    ) -> Observation;
}

struct Target {
    result: TargetResult,
    scope: Option<AppScope>,
    handle: Option<usize>,
    sent_at: Option<Instant>,
    finished: bool,
}

pub struct Session {
    store: Store,
    _execution: Option<ExecutionGuard>,
    backend: Box<dyn Backend>,
    plan: CleanupPlan,
    mode: CleanupMode,
    created: Instant,
    result: CleanupResult,
    targets: Vec<Target>,
    resource_started: Option<Instant>,
    last_resource_sample: Option<Instant>,
    finished: bool,
    stop_sending: bool,
    progress: String,
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    _test_root: Option<tests::TestRoot>,
}

fn now_ms() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "时间值溢出")?)
}

fn evaluated(
    snapshot: &Snapshot,
    state: &PolicyState,
    mode: &CleanupMode,
    available: bool,
) -> Result<CleanupPlan, String> {
    let mut scoped = state.clone();
    if let CleanupMode::Manual { group_id } = mode {
        let scope = policy::scope_for_group(snapshot, group_id)?;
        // One-shot consent is scoped to this session; never save an allow rule.
        scoped.rules.push(PolicyRule {
            id: "manual:session".into(),
            action: RuleAction::Allow,
            scope,
            source: "user".into(),
            enabled: true,
            created_at_unix_ms: snapshot.sampled_at_unix_ms,
        });
    }
    let mut plan = policy::evaluate(
        snapshot,
        &scoped,
        &PolicyContext {
            state_valid: true,
            a1_enabled: available,
        },
    );
    if let CleanupMode::Manual { group_id } = mode {
        plan.entries.retain(|entry| &entry.group_id == group_id);
        plan.automatic_count = plan
            .entries
            .iter()
            .filter(|e| e.disposition == Disposition::Automatic)
            .count();
        plan.pending_count = plan
            .entries
            .iter()
            .filter(|e| e.disposition == Disposition::Pending)
            .count();
        plan.protected_count = plan
            .entries
            .iter()
            .filter(|e| e.disposition == Disposition::Protected)
            .count();
    }
    Ok(plan)
}

impl Session {
    /// Freeze and durably record before tick can send a request. The lane remains held.
    pub fn start(store: Store, snapshot: Snapshot, mode: CleanupMode) -> Result<Self, String> {
        Self::with_backend(store, snapshot, mode, Box::new(NativeBackend::new()?))
    }
    fn with_backend(
        store: Store,
        snapshot: Snapshot,
        mode: CleanupMode,
        mut backend: Box<dyn Backend>,
    ) -> Result<Self, String> {
        let execution = store.execution_lock()?;
        let state = store.load()?;
        let started = now_ms()?;
        if started.abs_diff(snapshot.sampled_at_unix_ms) > PLAN_MAX_AGE.as_millis() as u64 {
            return Err("计划采样超过 30 秒；请刷新后重新触发".into());
        }
        let plan = evaluated(&snapshot, &state, &mode, backend.available())?;
        let mut targets = Vec::new();
        for entry in &plan.entries {
            let selected = matches!(mode, CleanupMode::Manual { .. })
                || entry.disposition == Disposition::Automatic
                || entry.matched_rule_ids.iter().any(|id| {
                    state
                        .rules
                        .iter()
                        .any(|r| &r.id == id && r.action == RuleAction::Allow && r.enabled)
                });
            if !selected {
                continue;
            }
            let mut target = Target {
                result: TargetResult {
                    group_id: entry.group_id.clone(),
                    name: crate::model::safe_text(&entry.name),
                    identity: entry.target_identity.clone(),
                    outcome: Outcome::Skipped,
                    reason: entry.reasons.join("；"),
                    request_sent: false,
                    observed_ms: 0,
                },
                scope: None,
                handle: None,
                sent_at: None,
                finished: true,
            };
            if entry.disposition == Disposition::Automatic {
                match policy::scope_for_group(&snapshot, &entry.group_id).and_then(|scope| {
                    let identity = entry.target_identity.as_ref().ok_or("实例标记缺失")?;
                    backend
                        .freeze(identity, &scope)
                        .map(|handle| (scope, handle))
                }) {
                    Ok((scope, handle)) => {
                        target.scope = Some(scope);
                        target.handle = Some(handle);
                        target.finished = false;
                        target.result.outcome = Outcome::Unknown;
                        target.result.reason = "冻结实例，尚未发送请求".into();
                    }
                    Err(error) => target.result.reason = format!("控制对象冻结失败：{error}"),
                }
            }
            targets.push(target);
        }
        let run_id = format!("run:{}:{started}:{}", std::process::id(), plan.plan_id);
        let result = CleanupResult {
            schema_version: 1,
            run_id,
            plan_id: plan.plan_id.clone(),
            rule_revision: state.revision,
            started_at_unix_ms: started,
            finished_at_unix_ms: started,
            cancelled: false,
            targets: Vec::new(),
            resource_before: snapshot.system,
            resource_after: None,
            resource_observation: "尚未进行资源复查".into(),
            errors: Vec::new(),
        };
        store.append_record("cleanup_started", json!({"run_id":result.run_id,"plan_id":plan.plan_id,"rule_revision":state.revision,"target_count":targets.len(),"a1_enabled":backend.available(),"targets":targets.iter().map(|t| &t.result).collect::<Vec<_>>()}))?;
        Ok(Self {
            store,
            _execution: Some(execution),
            backend,
            plan,
            mode,
            created: Instant::now(),
            result,
            targets,
            resource_started: None,
            last_resource_sample: None,
            finished: false,
            stop_sending: false,
            progress: "计划已冻结，尚未发送请求".into(),
            cancelled: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            _test_root: None,
        })
    }
    pub fn plan(&self) -> &CleanupPlan {
        &self.plan
    }
    pub fn set_cancel_flag(&mut self, cancelled: Arc<AtomicBool>) {
        self.cancelled = cancelled;
    }
    pub fn progress(&self) -> &str {
        &self.progress
    }
    pub fn result(&self) -> Option<&CleanupResult> {
        self.finished.then_some(&self.result)
    }
    pub fn targets(&self) -> Vec<TargetResult> {
        self.targets
            .iter()
            .map(|target| target.result.clone())
            .collect()
    }
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }
    fn record(&mut self, event: &str, data: serde_json::Value) -> bool {
        if let Err(error) = self.store.append_record(event, data) {
            self.result.errors.push(format!(
                "{event} 记录失败：{error}；后续请求停止，已发请求继续观察"
            ));
            self.stop_sending = true;
            false
        } else {
            true
        }
    }
    fn tick_at(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        if self.cancelled.load(Ordering::Acquire) {
            self.cancel();
            return;
        }
        self.backend.pump();
        for index in 0..self.targets.len() {
            if self.targets[index].finished {
                continue;
            }
            let Some(sent) = self.targets[index].sent_at else {
                continue;
            };
            let target = &self.targets[index];
            let observed = self.backend.observe(
                target.handle.unwrap(),
                target.result.identity.as_ref().unwrap(),
                target.scope.as_ref().unwrap(),
            );
            let elapsed = Instant::now().max(now).saturating_duration_since(sent);
            self.targets[index].result.observed_ms =
                elapsed.as_millis().min(u64::MAX as u128) as u64;
            let outcome = match observed {
                Observation::Exited { restarted } => Some((
                    Outcome::Exited,
                    if restarted {
                        "原实例已退出；发现新实例，不重复发送请求".into()
                    } else {
                        "冻结的原实例已退出".into()
                    },
                )),
                Observation::Running if elapsed >= EXIT_WAIT => Some((
                    Outcome::StillRunning,
                    "15 秒内未观察到退出；可能等待保存或拒绝，不自动强退".into(),
                )),
                Observation::Unknown(error) if elapsed >= EXIT_WAIT => {
                    Some((Outcome::Unknown, format!("退出观察不完整：{error}")))
                }
                _ => None,
            };
            if let Some((outcome, reason)) = outcome {
                let target = &mut self.targets[index];
                target.finished = true;
                target.result.outcome = outcome;
                target.result.reason = reason;
                target.result.observed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
                let data = json!({"run_id":self.result.run_id,"target":target.result});
                self.record("target_request_outcome", data);
            }
        }
        if self.stop_sending {
            for target in &mut self.targets {
                if !target.finished && target.sent_at.is_none() {
                    target.finished = true;
                    target.result.outcome = Outcome::Skipped;
                    target.result.reason = "必要记录失败；未发送请求".into();
                }
            }
        } else if let Some(index) = self
            .targets
            .iter()
            .position(|t| !t.finished && t.sent_at.is_none())
        {
            self.send_next(index, now);
        }
        if self.finished {
            return;
        }
        let completed = self.targets.iter().filter(|t| t.finished).count();
        self.progress = format!(
            "已核验 {completed}/{} 项；已发送 {} 个请求。Ctrl+C 取消尚未发送项",
            self.targets.len(),
            self.targets
                .iter()
                .filter(|t| t.result.request_sent)
                .count()
        );
        if self.targets.iter().all(|t| t.finished) {
            if !self.targets.iter().any(|t| t.result.request_sent) {
                self.result.resource_observation = format!(
                    "没有发送退出请求，未进行 10 秒资源复查。{}",
                    capability_reason()
                );
                self.finish();
                return;
            }
            let start = *self.resource_started.get_or_insert(now);
            self.progress = "退出观察结束，正在进行 10 秒系统资源复查；变化不归因于 Bree".into();
            if self
                .last_resource_sample
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(2))
            {
                self.last_resource_sample = Some(now);
                match self.backend.snapshot() {
                    Ok(sample) => self.result.resource_after = Some(sample.system),
                    Err(error) => self.result.errors.push(format!("资源复查缺失：{error}")),
                }
            }
            if now.saturating_duration_since(start) >= RESOURCE_WINDOW {
                self.result.resource_observation = resource_text(
                    &self.result.resource_before,
                    self.result.resource_after.as_ref(),
                );
                self.finish();
            }
        }
    }
    fn send_next(&mut self, index: usize, now: Instant) {
        enum Dispatch {
            Cancelled,
            LogFailure,
            Requested(Result<bool, String>),
        }
        // The rule writer uses the same state -> journal lock order. Protection can
        // commit before this critical section or after dispatch, never in its gap.
        let store = self.store.clone();
        let dispatch = store.with_state_lock(|state| {
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(Dispatch::Cancelled);
            }
            if now.saturating_duration_since(self.created) > PLAN_MAX_AGE {
                return Err("冻结计划超过 30 秒，新目标留待下一次触发".into());
            }
            if state.revision != self.plan.rule_revision {
                return Err("规则修订已变化；当前冻结计划不扩大或重新选择目标".into());
            }
            let sample = self.backend.snapshot()?;
            let fresh = evaluated(&sample, state, &self.mode, self.backend.available())?;
            let target = &self.targets[index];
            let entry = fresh
                .entries
                .iter()
                .find(|e| e.group_id == target.result.group_id)
                .ok_or("原组实例已退出或变化")?;
            if entry.disposition != Disposition::Automatic
                || entry.target_identity != target.result.identity
            {
                return Err(format!("执行重验未通过：{}", entry.reasons.join("；")));
            }
            if policy::scope_for_group(&sample, &entry.group_id)? != *target.scope.as_ref().unwrap()
            {
                return Err("安装范围已变化".into());
            }
            // A crash after preparation is Unknown, never a replay instruction.
            if !self.record(
                "target_request_prepared",
                json!({"run_id":self.result.run_id,"target":self.targets[index].result}),
            ) {
                return Ok(Dispatch::LogFailure);
            }
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(Dispatch::Cancelled);
            }
            if Instant::now().saturating_duration_since(self.created) > PLAN_MAX_AGE {
                return Err("必要记录等待后计划超过 30 秒，未发送请求".into());
            }
            let target = &self.targets[index];
            Ok(Dispatch::Requested(self.backend.request(
                target.handle.unwrap(),
                target.result.identity.as_ref().unwrap(),
                target.scope.as_ref().unwrap(),
                &self.cancelled,
            )))
        });
        let requested = match dispatch {
            Ok(Dispatch::Cancelled) => {
                self.cancel();
                return;
            }
            Ok(Dispatch::LogFailure) => return,
            Ok(Dispatch::Requested(requested)) => requested,
            Err(error) => {
                if self.cancelled.load(Ordering::Acquire) {
                    self.cancel();
                    return;
                }
                let target = &mut self.targets[index];
                target.finished = true;
                target.result.reason = error;
                target.result.outcome = Outcome::Skipped;
                self.record(
                    "target_request_outcome",
                    json!({"run_id":self.result.run_id,"target":self.targets[index].result}),
                );
                return;
            }
        };
        if requested.is_err() && self.cancelled.load(Ordering::Acquire) {
            self.cancel();
            return;
        }
        let target = &mut self.targets[index];
        match requested {
            Ok(true) => {
                target.result.request_sent = true;
                // The observation budget starts after the call accepts the request,
                // rather than before state/log waits and native revalidation.
                target.sent_at = Some(Instant::now().max(now));
                target.result.reason = "请求已发送，等待实际退出观察；未完成核验".into();
            }
            Ok(false) => {
                target.finished = true;
                target.result.outcome = Outcome::RequestRefused;
                target.result.reason = "正常退出请求未被接受；不自动强退".into();
            }
            Err(error) => {
                target.finished = true;
                target.result.outcome = Outcome::Skipped;
                target.result.reason = format!("原生重验或请求失败：{error}");
            }
        }
        self.record(
            "target_request_sent",
            json!({"run_id":self.result.run_id,"target":self.targets[index].result}),
        );
        if self.cancelled.load(Ordering::Acquire) {
            self.cancel();
        }
    }
    pub fn cancel(&mut self) {
        if self.finished {
            return;
        }
        self.result.cancelled = true;
        for target in &mut self.targets {
            if !target.finished {
                if let Some(sent) = target.sent_at {
                    target.result.observed_ms = target.result.observed_ms.max(
                        Instant::now()
                            .saturating_duration_since(sent)
                            .as_millis()
                            .min(u64::MAX as u128) as u64,
                    );
                }
                target.finished = true;
                if target.result.request_sent {
                    target.result.outcome = Outcome::Unknown;
                    target.result.reason =
                        "取消时已发送请求但未完成退出核验；不能撤销已发请求".into();
                } else {
                    target.result.outcome = Outcome::Cancelled;
                    target.result.reason = "取消时尚未发送请求".into();
                }
            }
        }
        self.result.resource_observation = "用户取消，资源观察窗口不完整；已发请求不能撤销".into();
        self.finish();
    }
    fn finish(&mut self) {
        self.result.targets = self.targets.iter().map(|t| t.result.clone()).collect();
        self.result.finished_at_unix_ms = now_ms().unwrap_or(self.result.started_at_unix_ms);
        let data = serde_json::to_value(&self.result).expect("serializable result");
        self.record("cleanup_finished", data);
        self.finished = true;
        self.progress = "处理已结束，退出事实与资源观察分开记录".into();
        self._execution.take();
    }
}

pub fn resource_text(before: &SystemMemory, after: Option<&SystemMemory>) -> String {
    let Some(after) = after else {
        return "10 秒窗口没有有效复查数据，不能判断系统资源变化".into();
    };
    let memory = match (before.used_bytes.value, after.used_bytes.value) {
        (Some(a), Some(b)) if b < a => format!("系统已用观察到减少 {} bytes", a - b),
        (Some(a), Some(b)) if b > a => format!("系统已用观察到增加 {} bytes", b - a),
        (Some(_), Some(_)) => "系统已用未变化".into(),
        _ => "系统已用缺失，不能比较".into(),
    };
    format!(
        "10 秒复查：{memory}；压力 {} → {}。自然波动不归因于 Bree，未改善也是合法结果",
        crate::output::pressure_text(&before.pressure),
        crate::output::pressure_text(&after.pressure)
    )
}

#[cfg(target_os = "macos")]
#[path = "platform/actions_macos.rs"]
mod native;
#[cfg(target_os = "macos")]
use native::NativeBackend;
#[cfg(not(target_os = "macos"))]
struct NativeBackend;
#[cfg(not(target_os = "macos"))]
impl NativeBackend {
    fn new() -> Result<Self, String> {
        Err("正常退出只面向已验证 macOS 环境".into())
    }
}
#[cfg(not(target_os = "macos"))]
impl Backend for NativeBackend {
    fn available(&self) -> bool {
        false
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        Err("平台不支持".into())
    }
    fn freeze(&mut self, _: &ProcessIdentity, _: &AppScope) -> Result<usize, String> {
        Err("平台不支持".into())
    }
    fn request(
        &mut self,
        _: usize,
        _: &ProcessIdentity,
        _: &AppScope,
        _: &AtomicBool,
    ) -> Result<bool, String> {
        Err("平台不支持".into())
    }
    fn observe(&mut self, _: usize, _: &ProcessIdentity, _: &AppScope) -> Observation {
        Observation::Unknown("平台不支持".into())
    }
}

#[cfg(test)]
impl Session {
    /// A safe UI fixture: only an in-memory backend, no AppKit or real process requests.
    pub(crate) fn test_running() -> Self {
        let root = tests::TestRoot::new();
        let sample = tests::sample(1);
        let store = root.store();
        tests::allow(&store, &sample);
        let (backend, _) = tests::fake(sample.clone());
        let mut session =
            Self::with_backend(store, sample, CleanupMode::Automatic, backend).unwrap();
        session._test_root = Some(root);
        session.tick_at(Instant::now());
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::policy::RuleChange;
    use std::{
        cell::RefCell,
        collections::HashMap,
        path::PathBuf,
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
    };
    pub(super) struct TestRoot(PathBuf);
    impl TestRoot {
        pub(super) fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "bree-execution-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            )))
        }
        pub(super) fn store(&self) -> Store {
            Store::at(self.0.clone())
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    pub(super) fn sample(count: u32) -> Snapshot {
        let mut sample = Snapshot {
            schema_version: 1,
            sampled_at_unix_ms: now_ms().unwrap(),
            collected_in_ms: 1,
            system: SystemMemory {
                total_bytes: Metric::ok(1000, "test"),
                used_bytes: Metric::ok(500, "test"),
                compressed_bytes: Metric::ok(0, "test"),
                swap_used_bytes: Metric::ok(0, "test"),
                cached_bytes: Metric::ok(0, "test"),
                pressure: Metric::ok(Pressure::Normal, "test"),
                used_definition: "test".into(),
            },
            processes: vec![],
            groups: vec![],
            coverage: Coverage {
                enumerated_processes: count as usize,
                readable_memory_processes: count as usize,
                reliable_identity_processes: count as usize,
                notes: vec![],
            },
            diagnostics: vec![],
        };
        for n in 0..count {
            let identity = ProcessIdentity {
                boot_session: "fixture-boot".into(),
                pid: 100 + n,
                start_seconds: Some(5),
                start_microseconds: Some(n),
                status: Validity::Ok,
            };
            let id = identity.object_id();
            let name = format!("Fixture{n}");
            let bundle = format!("/Applications/{name}.app");
            sample.processes.push(ProcessInfo {
                id: id.clone(),
                identity,
                parent_pid: Some(1),
                uid: Some(unsafe { libc::geteuid() }),
                name: name.clone(),
                executable_path: Some(format!("{bundle}/Contents/MacOS/{name}")),
                memory_bytes: Metric::ok(20, "test_rss"),
                metric_kind: "rss".into(),
                cpu_one_core_percent: Metric::ok(1.0, "test"),
                category: Category::Application,
                attribution: Attribution {
                    application: Some(Application {
                        bundle_id: Some(format!("dev.bree.fixture{n}")),
                        bundle_path: bundle,
                        name: name.clone(),
                        leader_pid: 100 + n,
                        frontmost: false,
                    }),
                    method: "appkit_main_application".into(),
                    confidence: "high".into(),
                    explanation: "fake exact main instance".into(),
                },
                protection_reasons: vec![],
                quit_supported: true,
            });
            sample.groups.push(OccupancyGroup {
                id: format!("app:{id}"),
                name,
                category: Category::Application,
                memory_bytes: Metric::ok(20, "test_rss"),
                metric_kind: "rss".into(),
                process_ids: vec![id],
                explanation: "fixture".into(),
            });
        }
        sample
    }
    pub(super) fn allow(store: &Store, sample: &Snapshot) {
        for group in &sample.groups {
            store
                .change(RuleChange::Add {
                    action: RuleAction::Allow,
                    scope: policy::scope_for_group(sample, &group.id).unwrap(),
                })
                .unwrap();
        }
    }
    #[derive(Clone)]
    enum FakeObservation {
        Running,
        Exited(bool),
        Unknown,
    }
    pub(super) struct FakeState {
        sample: Snapshot,
        requests: Vec<String>,
        freezes: Vec<String>,
        observe: HashMap<String, FakeObservation>,
        refuse: Vec<String>,
        journal_failure: Option<PathBuf>,
        on_snapshot: Option<Box<dyn FnOnce()>>,
        on_request: Option<Box<dyn FnOnce()>>,
    }
    struct FakeBackend(Rc<RefCell<FakeState>>);
    pub(super) fn fake(sample: Snapshot) -> (Box<dyn Backend>, Rc<RefCell<FakeState>>) {
        let state = Rc::new(RefCell::new(FakeState {
            sample,
            requests: vec![],
            freezes: vec![],
            observe: HashMap::new(),
            refuse: vec![],
            journal_failure: None,
            on_snapshot: None,
            on_request: None,
        }));
        (Box::new(FakeBackend(state.clone())), state)
    }
    impl Backend for FakeBackend {
        fn available(&self) -> bool {
            true
        }
        fn snapshot(&mut self) -> Result<Snapshot, String> {
            let callback = self.0.borrow_mut().on_snapshot.take();
            if let Some(callback) = callback {
                callback();
            }
            Ok(self.0.borrow().sample.clone())
        }
        fn freeze(&mut self, identity: &ProcessIdentity, _: &AppScope) -> Result<usize, String> {
            let mut state = self.0.borrow_mut();
            let n = state.freezes.len();
            state.freezes.push(identity.object_id());
            Ok(n)
        }
        fn request(
            &mut self,
            handle: usize,
            identity: &ProcessIdentity,
            _: &AppScope,
            cancelled: &AtomicBool,
        ) -> Result<bool, String> {
            let callback = self.0.borrow_mut().on_request.take();
            if let Some(callback) = callback {
                callback();
            }
            if cancelled.load(Ordering::Acquire) {
                return Err("请求前取消".into());
            }
            let mut state = self.0.borrow_mut();
            assert_eq!(state.freezes[handle], identity.object_id());
            state.requests.push(identity.object_id());
            if let Some(root) = state.journal_failure.take() {
                std::fs::remove_file(root.join("journal.jsonl")).unwrap();
                std::fs::create_dir(root.join("journal.jsonl")).unwrap();
            }
            Ok(!state.refuse.contains(&identity.object_id()))
        }
        fn observe(
            &mut self,
            handle: usize,
            identity: &ProcessIdentity,
            _: &AppScope,
        ) -> Observation {
            let state = self.0.borrow();
            assert_eq!(state.freezes[handle], identity.object_id());
            match state
                .observe
                .get(&identity.object_id())
                .unwrap_or(&FakeObservation::Running)
            {
                FakeObservation::Running => Observation::Running,
                FakeObservation::Exited(restarted) => Observation::Exited {
                    restarted: *restarted,
                },
                FakeObservation::Unknown => Observation::Unknown("fixture unreadable".into()),
            }
        }
    }
    fn session(root: &TestRoot, sample: Snapshot) -> (Session, Rc<RefCell<FakeState>>) {
        let store = root.store();
        allow(&store, &sample);
        let (backend, state) = fake(sample.clone());
        (
            Session::with_backend(store, sample, CleanupMode::Automatic, backend).unwrap(),
            state,
        )
    }
    #[test]
    fn no_rules_is_zero_without_requests_or_claimed_resource_improvement() {
        let root = TestRoot::new();
        let sample = sample(1);
        let (backend, state) = fake(sample.clone());
        let mut session =
            Session::with_backend(root.store(), sample, CleanupMode::Automatic, backend).unwrap();
        session.tick();
        let result = session.result().unwrap();
        assert_eq!(result.exit_code(), 0);
        assert!(result.targets.is_empty());
        assert!(result.resource_after.is_none());
        assert!(state.borrow().requests.is_empty());
        assert_eq!(
            root.store()
                .records(100)
                .unwrap()
                .iter()
                .filter(|r| r.event == "cleanup_finished")
                .count(),
            1
        );
    }
    #[test]
    fn request_success_is_not_exit_and_resource_increase_is_a_valid_result() {
        let root = TestRoot::new();
        let sample = sample(1);
        let id = sample.processes[0].id.clone();
        let (mut session, state) = session(&root, sample);
        let start = Instant::now();
        session.tick_at(start);
        assert!(session.result().is_none());
        assert!(session.targets()[0].request_sent);
        assert_eq!(session.targets()[0].outcome, Outcome::Unknown);
        {
            let mut state = state.borrow_mut();
            state.observe.insert(id, FakeObservation::Exited(false));
            state.sample.system.used_bytes.value = Some(700);
        }
        session.tick_at(start + Duration::from_secs(1));
        assert!(session.result().is_none());
        session.tick_at(start + Duration::from_secs(11));
        let result = session.result().unwrap();
        assert_eq!(result.targets[0].outcome, Outcome::Exited);
        assert_eq!(result.exit_code(), 0);
        assert!(result.resource_observation.contains("增加 200"));
        assert_eq!(state.borrow().requests.len(), 1);
        assert_eq!(state.borrow().freezes.len(), 1);
    }
    #[test]
    fn refusal_and_timeout_do_not_escalate_or_block_other_request_display() {
        let root = TestRoot::new();
        let sample = sample(2);
        let (mut session, state) = session(&root, sample.clone());
        state
            .borrow_mut()
            .refuse
            .push(sample.processes[0].id.clone());
        let start = Instant::now();
        session.tick_at(start);
        session.tick_at(start + Duration::from_millis(100));
        assert_eq!(session.targets()[0].outcome, Outcome::RequestRefused);
        assert!(session.targets()[1].request_sent);
        let sent = session.targets[1].sent_at.unwrap();
        session.tick_at(sent + EXIT_WAIT);
        session.tick_at(sent + EXIT_WAIT + RESOURCE_WINDOW);
        let result = session.result().unwrap();
        assert_eq!(result.exit_code(), 3);
        assert_eq!(result.targets[1].outcome, Outcome::StillRunning);
        assert_eq!(state.borrow().requests.len(), 2);
    }
    #[test]
    fn changed_foreground_skips_without_adding_new_targets() {
        let root = TestRoot::new();
        let sample = sample(1);
        let (mut session, state) = session(&root, sample);
        state.borrow_mut().sample.processes[0]
            .attribution
            .application
            .as_mut()
            .unwrap()
            .frontmost = true;
        session.tick();
        assert_eq!(session.result().unwrap().exit_code(), 4);
        assert!(state.borrow().requests.is_empty());
        assert_eq!(session.targets().len(), 1);
    }
    #[test]
    fn changed_rule_revision_skips_without_replanning_or_widening() {
        let root = TestRoot::new();
        let sample = sample(1);
        let scope = policy::scope_for_group(&sample, &sample.groups[0].id).unwrap();
        let (mut session, state) = session(&root, sample);
        root.store()
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope,
            })
            .unwrap();
        session.tick();
        assert_eq!(session.result().unwrap().exit_code(), 4);
        assert!(session.targets()[0].reason.contains("修订已变化"));
        assert!(state.borrow().requests.is_empty());
    }
    #[test]
    fn cancellation_during_required_journal_wait_prevents_dispatch() {
        use std::{fs::OpenOptions, os::fd::AsRawFd, sync::mpsc, thread};
        let root = TestRoot::new();
        let (mut session, state) = session(&root, sample(1));
        let cancelled = Arc::new(AtomicBool::new(false));
        session.set_cancel_flag(cancelled.clone());
        let path = root.0.join("journal.lock");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            thread::sleep(Duration::from_millis(30));
            cancelled.store(true, Ordering::Release);
            drop(file);
        });
        ready_rx.recv().unwrap();
        state.borrow_mut().on_snapshot = Some(Box::new(move || go_tx.send(()).unwrap()));
        session.tick();
        worker.join().unwrap();
        assert!(state.borrow().requests.is_empty());
        let result = session.result().unwrap();
        assert_eq!(result.exit_code(), 130);
        assert_eq!(result.targets[0].outcome, Outcome::Cancelled);
        assert!(
            root.store()
                .records(20)
                .unwrap()
                .iter()
                .any(|r| r.event == "target_request_prepared")
        );
        assert!(root.store().execution_lock().is_ok());
    }
    #[test]
    fn cancellation_in_final_native_revalidation_prevents_dispatch() {
        let root = TestRoot::new();
        let (mut session, state) = session(&root, sample(1));
        let cancelled = Arc::new(AtomicBool::new(false));
        session.set_cancel_flag(cancelled.clone());
        state.borrow_mut().on_request =
            Some(Box::new(move || cancelled.store(true, Ordering::Release)));
        session.tick();
        assert!(state.borrow().requests.is_empty());
        assert_eq!(session.result().unwrap().exit_code(), 130);
        assert_eq!(session.targets()[0].outcome, Outcome::Cancelled);
    }
    #[test]
    fn rule_commit_cannot_pass_final_validation_during_journal_wait() {
        use std::{fs::OpenOptions, os::fd::AsRawFd, sync::mpsc, thread};
        let root = TestRoot::new();
        let sample = sample(1);
        let scope = policy::scope_for_group(&sample, &sample.groups[0].id).unwrap();
        let (mut session, state) = session(&root, sample);
        let store = root.store();
        let path = root.0.join("journal.lock");
        let committed = Arc::new(AtomicBool::new(false));
        let worker_committed = committed.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            thread::sleep(Duration::from_millis(30));
            // Allow the journal write to progress, then try to commit protection.
            // The executioner's state lock still encloses validation and dispatch.
            drop(file);
            store
                .change(RuleChange::Add {
                    action: RuleAction::Protect,
                    scope,
                })
                .unwrap();
            worker_committed.store(true, Ordering::Release);
        });
        ready_rx.recv().unwrap();
        state.borrow_mut().on_snapshot = Some(Box::new(move || go_tx.send(()).unwrap()));
        let check_store = root.store();
        state.borrow_mut().on_request = Some(Box::new(move || {
            assert!(!committed.load(Ordering::Acquire));
            assert_eq!(check_store.load().unwrap().revision, 1);
        }));
        let start = Instant::now();
        session.tick_at(start);
        worker.join().unwrap();
        assert_eq!(state.borrow().requests.len(), 1);
        assert_eq!(root.store().load().unwrap().revision, 2);
        let sent = session.targets[0].sent_at.unwrap();
        session.tick_at(start + EXIT_WAIT);
        assert_eq!(
            session.targets()[0].outcome,
            Outcome::Unknown,
            "the journal wait must not consume the request's 15-second observation budget"
        );
        session.tick_at(sent + EXIT_WAIT);
        assert_eq!(session.targets()[0].outcome, Outcome::StillRunning);
        session.cancel();
    }
    #[test]
    fn pid_replacement_before_request_is_skipped_not_rebound() {
        let root = TestRoot::new();
        let sample = sample(1);
        let (mut session, state) = session(&root, sample);
        {
            let mut state = state.borrow_mut();
            let process = &mut state.sample.processes[0];
            process.identity.start_microseconds = Some(99);
            process.id = process.identity.object_id();
            let id = process.id.clone();
            state.sample.groups[0].id = format!("app:{id}");
            state.sample.groups[0].process_ids = vec![id];
        }
        session.tick();
        assert_eq!(session.targets()[0].outcome, Outcome::Skipped);
        assert!(state.borrow().requests.is_empty());
        assert_eq!(state.borrow().freezes.len(), 1);
    }
    #[test]
    fn restart_after_request_is_reported_once_never_requested_again() {
        let root = TestRoot::new();
        let sample = sample(1);
        let id = sample.processes[0].id.clone();
        let (mut session, state) = session(&root, sample);
        let start = Instant::now();
        session.tick_at(start);
        state
            .borrow_mut()
            .observe
            .insert(id, FakeObservation::Exited(true));
        session.tick_at(start + Duration::from_secs(1));
        session.tick_at(start + Duration::from_secs(11));
        session.tick_at(start + Duration::from_secs(12));
        assert!(session.targets()[0].reason.contains("新实例"));
        assert_eq!(state.borrow().requests.len(), 1);
    }
    #[test]
    fn unreadable_observation_is_unknown_after_timeout() {
        let root = TestRoot::new();
        let sample = sample(1);
        let id = sample.processes[0].id.clone();
        let (mut session, state) = session(&root, sample);
        state
            .borrow_mut()
            .observe
            .insert(id, FakeObservation::Unknown);
        let start = Instant::now();
        session.tick_at(start);
        let sent = session.targets[0].sent_at.unwrap();
        session.tick_at(sent + EXIT_WAIT);
        session.tick_at(sent + EXIT_WAIT + RESOURCE_WINDOW);
        assert_eq!(
            session.result().unwrap().targets[0].outcome,
            Outcome::Unknown
        );
        assert_eq!(session.result().unwrap().exit_code(), 3);
    }
    #[test]
    fn failed_required_log_prevents_request_and_finished_success() {
        let root = TestRoot::new();
        let (mut session, state) = session(&root, sample(1));
        std::fs::remove_file(root.0.join("journal.jsonl")).unwrap();
        std::fs::create_dir(root.0.join("journal.jsonl")).unwrap();
        session.tick();
        session.tick();
        assert!(state.borrow().requests.is_empty());
        assert_eq!(session.result().unwrap().exit_code(), 1);
        assert!(root.0.join("journal.jsonl").is_dir());
    }
    #[test]
    fn post_request_log_failure_stops_new_requests_but_observes_sent_one() {
        let root = TestRoot::new();
        let sample = sample(2);
        let id = sample.processes[0].id.clone();
        let (mut session, state) = session(&root, sample);
        state.borrow_mut().journal_failure = Some(root.0.clone());
        let start = Instant::now();
        session.tick_at(start);
        state
            .borrow_mut()
            .observe
            .insert(id, FakeObservation::Exited(false));
        session.tick_at(start + Duration::from_secs(1));
        session.tick_at(start + Duration::from_secs(11));
        assert_eq!(state.borrow().requests.len(), 1);
        assert_eq!(session.targets()[0].outcome, Outcome::Exited);
        assert_eq!(session.targets()[1].outcome, Outcome::Skipped);
        assert_eq!(session.result().unwrap().exit_code(), 1);
    }
    #[test]
    fn cancel_keeps_sent_unknown_and_unsent_cancelled_then_releases_lane() {
        let root = TestRoot::new();
        let (mut session, state) = session(&root, sample(2));
        session.tick();
        assert!(root.store().execution_lock().is_err());
        session.cancel();
        session.tick();
        let result = session.result().unwrap();
        assert_eq!(result.exit_code(), 130);
        assert_eq!(result.targets[0].outcome, Outcome::Unknown);
        assert_eq!(result.targets[1].outcome, Outcome::Cancelled);
        assert_eq!(state.borrow().requests.len(), 1);
        assert!(root.store().execution_lock().is_ok());
        assert_eq!(
            crate::history::load(&root.store(), 10).unwrap()[0].status,
            "cancelled"
        );
    }
    #[test]
    fn abandoned_session_is_unfinished_history_never_replayed() {
        let root = TestRoot::new();
        let (session, state) = session(&root, sample(1));
        drop(session);
        let history = crate::history::load(&root.store(), 10).unwrap();
        assert_eq!(history[0].status, "unfinished");
        assert!(history[0].result.is_none());
        assert!(state.borrow().requests.is_empty());
        assert!(root.store().execution_lock().is_ok());
    }
    #[test]
    fn manual_consent_is_one_instance_and_never_persists_allow() {
        let root = TestRoot::new();
        let sample = sample(2);
        let (backend, state) = fake(sample.clone());
        let group_id = sample.groups[1].id.clone();
        let mut session = Session::with_backend(
            root.store(),
            sample,
            CleanupMode::Manual { group_id },
            backend,
        )
        .unwrap();
        session.tick();
        session.cancel();
        assert_eq!(state.borrow().requests.len(), 1);
        assert!(root.store().load().unwrap().rules.is_empty());
        assert_eq!(root.store().load().unwrap().revision, 0);
    }
    #[test]
    fn protection_overrides_single_use_manual_consent() {
        let root = TestRoot::new();
        let sample = sample(1);
        let scope = policy::scope_for_group(&sample, &sample.groups[0].id).unwrap();
        root.store()
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope,
            })
            .unwrap();
        let (backend, state) = fake(sample.clone());
        let mut session = Session::with_backend(
            root.store(),
            sample.clone(),
            CleanupMode::Manual {
                group_id: sample.groups[0].id.clone(),
            },
            backend,
        )
        .unwrap();
        session.tick();
        assert_eq!(session.result().unwrap().exit_code(), 4);
        assert!(state.borrow().requests.is_empty());
    }
    #[test]
    fn stale_plan_and_missing_comparison_metrics_never_look_valid() {
        let root = TestRoot::new();
        let mut sample = sample(1);
        sample.sampled_at_unix_ms -= 31000;
        let (backend, _) = fake(sample.clone());
        assert!(
            Session::with_backend(
                root.store(),
                sample.clone(),
                CleanupMode::Automatic,
                backend
            )
            .is_err()
        );
        sample.system.used_bytes = Metric::unavailable(Validity::Denied, "test", "denied");
        assert!(resource_text(&sample.system, Some(&sample.system)).contains("缺失"));
        assert!(!enabled());
    }
}
