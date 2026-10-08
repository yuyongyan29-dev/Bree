//! P2-only native experiment. Included by cleanup.rs only under cfg(test) on macOS.
//! Every control permit belongs to a Child spawned below, with a complete kernel
//! identity. The real Session and NativeBackend perform freeze/request/observe.
use super::*;
use objc2_app_kit::NSWorkspace;
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
};

const ORIGINAL: &str = "Bree P2 original.\n";
const EDITED: &str = "Bree P2 edited text.\n";

struct OwnedFixture {
    child: Child,
    identity: ProcessIdentity,
    scope: AppScope,
    root: PathBuf,
    log: PathBuf,
    documents: PathBuf,
    started: Instant,
}

fn event(case: &str, name: &str, data: Value) {
    println!(
        "{}",
        json!({"case":case,"event":name,"timestamp_unix_ms":now_ms().unwrap_or(0),"data":data})
    );
}

fn pump() {
    crate::collect::pump_platform_events();
    thread::sleep(Duration::from_millis(25));
}

fn fixture_events(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn has_event(log: &Path, wanted: &str) -> bool {
    fixture_events(log).iter().any(|row| row["event"] == wanted)
}

fn has_field(log: &Path, wanted: &str, field: &str, value: Value) -> bool {
    fixture_events(log)
        .iter()
        .any(|row| row["event"] == wanted && row[field] == value)
}

fn recovery_seen(log: &Path) -> bool {
    has_event(log, "safety_timer_natural_exit") || has_event(log, "p2_cleanup_exit")
}

fn frontmost_pid() -> Option<u32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier() as u32)
}

fn file_evidence(documents: &Path) -> Value {
    let path = documents.join("original.txt");
    let bytes = fs::read(&path);
    let digest = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(&path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.split_whitespace().next().map(str::to_owned));
    match bytes {
        Ok(bytes) => json!({
            "path":path,"present":true,"bytes":bytes.len(),
            "sha256":digest,"utf8_contents":String::from_utf8(bytes).ok()
        }),
        Err(error) => {
            json!({"path":path,"present":false,"error":error.to_string(),"sha256":digest})
        }
    }
}

impl OwnedFixture {
    fn spawn(executable: &Path, run_root: &Path, case: &str, mode: &str) -> Result<Self, String> {
        let root = run_root.join(case);
        if root.exists() {
            return Err(format!(
                "{case}: refusing to reuse an existing case directory"
            ));
        }
        let documents = root.join("documents");
        fs::create_dir(&root)
            .map_err(|error| format!("Cannot create fresh case directory {case}: {error}"))?;
        fs::create_dir_all(&documents).map_err(|error| error.to_string())?;
        fs::write(documents.join("original.txt"), ORIGINAL).map_err(|error| error.to_string())?;
        let log = root.join("fixture.jsonl");
        // There is no user-provided PID route. This direct Child is the only target.
        let child = Command::new(executable)
            .arg(mode)
            .arg(&log)
            .arg(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let started = Instant::now();
        let pid = child.id();
        let placeholder = ProcessIdentity {
            boot_session: String::new(),
            pid,
            start_seconds: None,
            start_microseconds: None,
            status: crate::model::Validity::Unknown,
        };
        let mut owned = Self {
            child,
            identity: placeholder,
            scope: AppScope {
                bundle_id: String::new(),
                bundle_path: String::new(),
                executable_path: executable.to_string_lossy().into_owned(),
            },
            root,
            log,
            documents,
            started,
        };
        // Record the owned PID before waiting for AppKit's launched event.
        let mut manifest = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_root.join("owned-children.jsonl"))
            .map_err(|error| error.to_string())?;
        writeln!(
            manifest,
            "{}",
            json!({"pid":pid,"executable":executable,"case":case})
        )
        .and_then(|_| manifest.flush())
        .map_err(|error| error.to_string())?;
        let mut collector = Collector::new()?;
        loop {
            pump();
            if owned
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(format!("{case}: fixture exited before discovery"));
            }
            let sample = collector.snapshot()?;
            if has_event(&owned.log, "launched")
                && let Some(process) = sample.processes.iter().find(|process| {
                    process.identity.pid == pid
                        && process.parent_pid == Some(std::process::id())
                        && process.executable_path.as_deref() == executable.to_str()
                        && policy::reliable_identity(process)
                })
                && let Ok(scope) = policy::scope_for_group(&sample, &format!("app:{}", process.id))
            {
                owned.identity = process.identity.clone();
                owned.scope = scope;
                event(
                    case,
                    "fixture_ready",
                    json!({
                        "identity":owned.identity,"scope":owned.scope,
                        "parent_pid_verified":true,"foreground_pid":frontmost_pid(),
                        "production_quit_supported":process.quit_supported,
                        "file":file_evidence(&owned.documents)
                    }),
                );
                return Ok(owned);
            }
            if started.elapsed() > Duration::from_secs(8) {
                return Err(format!(
                    "{case}: fixture not reliably discovered by production Collector"
                ));
            }
        }
    }

    fn alive(&mut self) -> Result<bool, String> {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|error| error.to_string())
    }

    fn start_session(&self) -> Result<Session, String> {
        let mut backend = NativeBackend::new_fixture(self.identity.clone(), self.scope.clone())?;
        let snapshot = backend.snapshot()?;
        let group_id = format!("app:{}", self.identity.object_id());
        // Manual mode supplies one-session consent and never writes an allow rule.
        Session::with_backend(
            Store::at(self.root.join("bree-data")),
            snapshot,
            CleanupMode::Manual { group_id },
            Box::new(backend),
        )
    }

    fn reap(&mut self, case: &str) -> Result<bool, String> {
        let was_alive = self.alive()?;
        if was_alive {
            // The fixture owns this explicit test-only self-exit protocol. All
            // Session results and file evidence must be frozen before this write.
            fs::write(self.root.join("reap"), b"P2 controller cleanup\n")
                .map_err(|error| error.to_string())?;
        }
        let waited = Instant::now();
        while self.alive()? {
            pump();
            if waited.elapsed() > Duration::from_secs(65) {
                return Err(format!(
                    "{case}: fixture did not complete self-recovery; no signal was sent"
                ));
            }
        }
        event(
            case,
            "fixture_reaped_after_evidence",
            json!({
                "was_alive":was_alive,"p2_cleanup_exit":has_event(&self.log,"p2_cleanup_exit"),
                "safety_timer_exit":has_event(&self.log,"safety_timer_natural_exit"),
            "counted_as_session_exit":false,"elapsed_ms":self.started.elapsed().as_millis(),
            "retained_edits_after_recovery":fs::read_to_string(self.root.join("retained-edits.txt")).ok(),
            "recovery_events":fixture_events(&self.log).into_iter().filter(|row|row["event"]=="p2_cleanup_exit" || row["event"]=="safety_timer_natural_exit").collect::<Vec<_>>()
            }),
        );
        Ok(true)
    }
}

impl Drop for OwnedFixture {
    fn drop(&mut self) {
        // Error paths also retain the experiment lock while children recover.
        // This cannot stop a user app: every Child was spawned by this controller.
        if let Ok(true) = self.alive() {
            let _ = fs::write(self.root.join("reap"), b"P2 error-path cleanup\n");
            let waited = Instant::now();
            while matches!(self.alive(), Ok(true)) && waited.elapsed() < Duration::from_secs(65) {
                pump();
            }
        }
    }
}

fn finish_session(session: &mut Session, fixture_log: &Path) -> Result<(), String> {
    let started = Instant::now();
    while session.result().is_none() {
        if recovery_seen(fixture_log) {
            session.cancel();
            return Err("Fixture recovery preceded the frozen Session result; it cannot count as quit success".into());
        }
        session.tick();
        pump();
        if started.elapsed() > Duration::from_secs(40) {
            session.cancel();
            return Err("Production Session exceeded its bounded experiment window".into());
        }
    }
    if recovery_seen(fixture_log) {
        return Err("Fixture recovery contaminated the frozen Session result".into());
    }
    Ok(())
}

fn ordinary_case(executable: &Path, run_root: &Path, case: &str) -> Result<bool, String> {
    let mut fixture = OwnedFixture::spawn(executable, run_root, case, case)?;
    let before = file_evidence(&fixture.documents);
    let mut session = fixture.start_session()?;
    event(
        case,
        "production_plan_frozen",
        json!({"plan":session.plan(),"targets":session.targets()}),
    );
    finish_session(&mut session, &fixture.log)?;
    let alive = fixture.alive()?;
    let after = file_evidence(&fixture.documents);
    let targets = session.targets();
    let target = targets.first().ok_or("Expected one exact fixture target")?;
    let fixture_reply_seen = has_event(&fixture.log, "application_should_terminate");
    let review_without_delegate = has_event(&fixture.log, "fixture_quit_apple_event_received")
        && has_event(&fixture.log, "document_review_started")
        && !fixture_reply_seen;
    let edits_retained = has_field(
        &fixture.log,
        "p2_retention_checkpoint",
        "memory_content",
        json!(EDITED),
    ) && fs::read_to_string(fixture.root.join("retained-edits.txt"))
        .ok()
        .as_deref()
        == Some(EDITED);
    let observation_consistent = !recovery_seen(&fixture.log)
        && match target.outcome {
            Outcome::Exited => !alive && has_event(&fixture.log, "will_terminate"),
            Outcome::StillRunning => alive && target.request_sent && target.observed_ms >= 15_000,
            Outcome::RequestRefused | Outcome::Skipped => alive,
            _ => false,
        };
    let content = after["utf8_contents"].as_str();
    let expected = match case {
        "immediate" | "later" => target.outcome == Outcome::Exited && fixture_reply_seen,
        "cancel" => {
            alive
                && fixture_reply_seen
                && target.outcome != Outcome::Exited
                && has_event(&fixture.log, "reply_cancel")
        }
        "cancel-later" => {
            alive
                && fixture_reply_seen
                && target.outcome != Outcome::Exited
                && has_event(&fixture.log, "reply_later_cancel")
        }
        "file-save" => {
            before["utf8_contents"] == ORIGINAL
                && content == Some(EDITED)
                && target.outcome == Outcome::Exited
                && fixture_reply_seen
                && has_field(&fixture.log, "file_save_completed", "success", json!(true))
        }
        "file-fail" => {
            before["utf8_contents"] == ORIGINAL
                && content == Some(ORIGINAL)
                && alive
                && review_without_delegate
                && edits_retained
                && target.outcome != Outcome::Exited
                && has_field(&fixture.log, "file_save_completed", "success", json!(false))
        }
        "file-panel-cancel" => {
            before["utf8_contents"] == ORIGINAL
                && content == Some(ORIGINAL)
                && alive
                && review_without_delegate
                && edits_retained
                && target.outcome != Outcome::Exited
                && has_event(&fixture.log, "file_save_panel_cancelled")
        }
        "file-wait" => {
            before["utf8_contents"] == ORIGINAL
                && content == Some(ORIGINAL)
                && alive
                && review_without_delegate
                && edits_retained
                && target.outcome == Outcome::StillRunning
                && target.observed_ms >= 15_000
                && has_event(&fixture.log, "document_review_started")
                && !has_event(&fixture.log, "document_review_button_clicked")
        }
        // This is a measured production-adapter comparison, with no fabricated
        // callback expectation. The report must keep its outcome distinct from
        // fixture mechanisms which install an explicit Quit AppleEvent handler.
        "file-save-plain" => {
            !has_event(&fixture.log, "fixture_quit_adapter_installed")
                && ((alive && content == Some(ORIGINAL)) || (!alive && content == Some(EDITED)))
        }
        _ => return Err(format!("Unknown P2 case: {case}")),
    };
    let passed = observation_consistent
        && expected
        && session
            .result()
            .is_some_and(|result| result.errors.is_empty());
    event(
        case,
        "frozen_evidence",
        json!({
            "passed":passed,"alive_before_recovery":alive,"file_before":before,"file_after":after,
            "session_result":session.result(),"fixture_events":fixture_events(&fixture.log),
        "fixture_reply_observed":fixture_reply_seen,
        "retained_edits_before_recovery":fs::read_to_string(fixture.root.join("retained-edits.txt")).ok(),
            "explicit_quit_adapter":has_event(&fixture.log,"fixture_quit_adapter_installed"),
            "production_gate_enabled":enabled(),"fixture_recovery_preceded_result":false,
            "plain_adapter_comparison":case=="file-save-plain",
            "save_choice_inferred_from_timeout":false
        }),
    );
    fixture.reap(case)?;
    Ok(passed)
}

fn foreground_case(executable: &Path, run_root: &Path) -> Result<bool, String> {
    let case = "foreground";
    let mut fixture = OwnedFixture::spawn(executable, run_root, case, case)?;
    let mut session = fixture.start_session()?;
    let frozen = session
        .targets()
        .first()
        .is_some_and(|target| target.outcome == Outcome::Unknown);
    // macOS 14+ does not promise that ignoringOtherApps grants foreground rights.
    // Ask LaunchServices to activate only this unique self-built installation.
    let activation = Command::new("/usr/bin/open")
        .arg("-a")
        .arg(&fixture.scope.bundle_path)
        .status()
        .map_err(|error| error.to_string())?;
    event(
        case,
        "waiting_for_foreground",
        json!({"pid":fixture.identity.pid,"app_path":fixture.scope.bundle_path,"maximum_seconds":20}),
    );
    let started = Instant::now();
    while frontmost_pid() != Some(fixture.identity.pid)
        && started.elapsed() < Duration::from_secs(20)
    {
        pump();
    }
    let became_frontmost = frontmost_pid() == Some(fixture.identity.pid);
    if became_frontmost {
        finish_session(&mut session, &fixture.log)?;
    } else {
        session.cancel();
    }
    let targets = session.targets();
    let passed = activation.success()
        && frozen
        && became_frontmost
        && session
            .result()
            .is_some_and(|result| result.errors.is_empty())
        && fixture.alive()?
        && targets.len() == 1
        && targets[0].outcome == Outcome::Skipped
        && !targets[0].request_sent
        && !has_event(&fixture.log, "application_should_terminate");
    event(
        case,
        "frozen_evidence",
        json!({
            "passed":passed,"frozen_while_background":frozen,"became_frontmost":became_frontmost,
            "session_result":session.result(),"fixture_events":fixture_events(&fixture.log),
            "foreground_request_sent":targets.iter().any(|target|target.request_sent)
        }),
    );
    fixture.reap(case)?;
    Ok(passed)
}

fn old_plan_case(executable: &Path, run_root: &Path) -> Result<bool, String> {
    let case = "old-plan";
    let mut original = OwnedFixture::spawn(executable, run_root, case, "immediate")?;
    let mut session = original.start_session()?;
    original.reap(case)?;
    let mut replacement =
        OwnedFixture::spawn(executable, run_root, "old-plan-replacement", "immediate")?;
    // The old fixture deliberately self-exited before dispatch. Its recovery is
    // not a Session exit observation; the stale plan must become Skipped.
    let started = Instant::now();
    while session.result().is_none() && started.elapsed() < Duration::from_secs(4) {
        session.tick();
        pump();
    }
    let targets = session.targets();
    let passed = session
        .result()
        .is_some_and(|result| result.errors.is_empty())
        && original.identity != replacement.identity
        && targets.len() == 1
        && targets[0].outcome == Outcome::Skipped
        && !targets[0].request_sent
        && replacement.alive()?
        && !has_event(&replacement.log, "application_should_terminate");
    event(
        case,
        "frozen_evidence",
        json!({
            "passed":passed,"old_identity":original.identity,"replacement_identity":replacement.identity,
            "session_result":session.result(),"replacement_alive":replacement.alive()?,
            "replacement_fixture_events":fixture_events(&replacement.log),
            "old_recovery_counted_as_quit":false,"replacement_requested":false
        }),
    );
    replacement.reap("old-plan-replacement")?;
    Ok(passed)
}

fn old_handle_case(executable: &Path, run_root: &Path) -> Result<bool, String> {
    let case = "old-handle";
    let mut original = OwnedFixture::spawn(executable, run_root, case, "immediate")?;
    let mut backend =
        NativeBackend::new_fixture(original.identity.clone(), original.scope.clone())?;
    let handle = backend.freeze(&original.identity, &original.scope)?;
    original.reap(case)?;
    let mut replacement =
        OwnedFixture::spawn(executable, run_root, "old-handle-replacement", "immediate")?;
    let request = backend.request(
        handle,
        &original.identity,
        &original.scope,
        &AtomicBool::new(false),
    );
    let passed = request.is_err()
        && replacement.alive()?
        && !has_event(&replacement.log, "application_should_terminate");
    event(
        case,
        "frozen_evidence",
        json!({
            "passed":passed,"native_request":request,"old_identity":original.identity,
            "replacement_identity":replacement.identity,"replacement_alive":replacement.alive()?,
            "replacement_fixture_events":fixture_events(&replacement.log),
            "old_recovery_counted_as_quit":false,"pid_rebound":false
        }),
    );
    replacement.reap("old-handle-replacement")?;
    Ok(passed)
}

fn restart_case(executable: &Path, run_root: &Path) -> Result<bool, String> {
    let case = "restart";
    let mut original = OwnedFixture::spawn(executable, run_root, case, "immediate")?;
    let mut session = original.start_session()?;
    // Tick once to dispatch; hold observation until the old Child exits and the
    // replacement is discoverable. This uses the same frozen production handle.
    session.tick();
    let started = Instant::now();
    while original.alive()? && started.elapsed() < Duration::from_secs(4) {
        pump();
    }
    if original.alive()? {
        session.cancel();
        event(
            case,
            "frozen_evidence",
            json!({
                "passed":false,"reason":"Original fixture did not exit following production dispatch",
                "session_result":session.result(),"fixture_events":fixture_events(&original.log)
            }),
        );
        original.reap(case)?;
        return Ok(false);
    }
    let mut replacement =
        OwnedFixture::spawn(executable, run_root, "restart-replacement", "immediate")?;
    finish_session(&mut session, &original.log)?;
    let targets = session.targets();
    let passed = session
        .result()
        .is_some_and(|result| result.errors.is_empty())
        && targets.len() == 1
        && targets[0].outcome == Outcome::Exited
        && targets[0].request_sent
        && targets[0].reason.contains("new instance was found")
        && replacement.alive()?
        && !has_event(&replacement.log, "application_should_terminate");
    event(
        case,
        "frozen_evidence",
        json!({
            "passed":passed,"session_result":session.result(),"original_identity":original.identity,
            "replacement_identity":replacement.identity,"replacement_alive":replacement.alive()?,
            "original_fixture_events":fixture_events(&original.log),
            "replacement_fixture_events":fixture_events(&replacement.log),"replacement_requested":false
        }),
    );
    replacement.reap("restart-replacement")?;
    Ok(passed)
}

/// Invoked only by the standalone test-linked main while its Python parent holds
/// the repository's native-experiment.lock for the complete child lifecycle.
pub fn run(executable: &Path, run_root: &Path) -> Result<bool, String> {
    if unsafe { libc::pthread_main_np() } == 0 {
        return Err("P2 controller must run on the process main thread".into());
    }
    if !executable.is_absolute() || !run_root.is_absolute() {
        return Err("P2 fixture and run paths must be absolute".into());
    }
    let executable = fs::canonicalize(executable).map_err(|error| error.to_string())?;
    let run_root = fs::canonicalize(run_root).map_err(|error| error.to_string())?;
    let p2_root = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .join(".artifacts/p2")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !run_root.starts_with(p2_root)
        || executable != run_root.join("build/BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture")
    {
        return Err(
            "P2 accepts only the newly self-built fixture inside its experiment directory".into(),
        );
    }
    event(
        "suite",
        "started",
        json!({
            "main_thread":true,"production_a1_enabled":enabled(),
            "test_only_library":true,"executable":executable,"run_root":run_root,
            "normal_exit_wait_seconds":EXIT_WAIT.as_secs(),"resource_window_seconds":RESOURCE_WINDOW.as_secs(),
            "real_user_apps_in_scope":false
        }),
    );
    let mut all_passed = true;
    let selected = std::env::args().nth(3);
    if selected.as_deref().is_some_and(|case| {
        ![
            "immediate",
            "later",
            "cancel",
            "cancel-later",
            "file-save",
            "file-fail",
            "file-panel-cancel",
            "file-wait",
            "file-save-plain",
            "foreground",
            "old-plan",
            "old-handle",
            "restart",
        ]
        .contains(&case)
    }) {
        return Err("Unknown P2 case".into());
    }
    for case in [
        "immediate",
        "later",
        "cancel",
        "cancel-later",
        "file-save",
        "file-fail",
        "file-panel-cancel",
        "file-wait",
        "file-save-plain",
    ] {
        if selected.as_deref().is_some_and(|selected| selected != case) {
            continue;
        }
        match ordinary_case(&executable, &run_root, case) {
            Ok(passed) => all_passed &= passed,
            Err(error) => {
                all_passed = false;
                event(case, "case_error", json!({"error":error,"passed":false}));
            }
        }
    }
    for (case, run_case) in [
        (
            "foreground",
            foreground_case as fn(&Path, &Path) -> Result<bool, String>,
        ),
        ("old-plan", old_plan_case),
        ("old-handle", old_handle_case),
        ("restart", restart_case),
    ] {
        if selected.as_deref().is_some_and(|selected| selected != case) {
            continue;
        }
        match run_case(&executable, &run_root) {
            Ok(passed) => all_passed &= passed,
            Err(error) => {
                all_passed = false;
                event(case, "case_error", json!({"error":error,"passed":false}));
            }
        }
    }
    event(
        "suite",
        "finished",
        json!({
            "all_case_assertions_passed":all_passed,"production_a1_enabled":enabled(),
            "ordinary_user_app_support_verified":false,"interface_instance_guarantee_verified":false,
            "recovery_exit_counted_as_quit":false,"force_termination_used":false
        }),
    );
    Ok(all_passed)
}
