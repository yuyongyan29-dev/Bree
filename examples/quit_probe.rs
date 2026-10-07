//! M0 / M3 experiment, separate from the Bree binary.
//! Only NSRunningApplication objects for children spawned here can receive a request.
//! No PID argument, signal, forceTerminate, automation permission, or user-app control.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("quit_probe requires macOS");
    std::process::exit(69);
}

#[cfg(target_os = "macos")]
fn main() {
    match macos::run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("quit_probe: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::{ClassType, msg_send, rc::Retained};
    use objc2_app_kit::{NSRunningApplication, NSWorkspace};
    use objc2_foundation::{NSDate, NSRunLoop};
    use serde::Serialize;
    use serde_json::{Value, json};
    use std::{
        error::Error,
        fs,
        os::unix::process::ExitStatusExt,
        path::{Path, PathBuf},
        process::{Child, Command},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    type Result<T> = std::result::Result<T, Box<dyn Error>>;
    const BUNDLE_ID: &str = "local.bree.m0.quit-fixture";
    const OBSERVE_SECONDS: u64 = 15;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    struct Identity {
        pid: i32,
        uid: u32,
        start_seconds: u64,
        start_microseconds: u64,
        executable: PathBuf,
    }

    struct OwnedFixture {
        child: Child,
        app: Retained<NSRunningApplication>,
        identity: Identity,
        started: Instant,
    }

    fn event(case: &str, name: &str, data: Value) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_secs_f64())
            .unwrap_or_default();
        println!(
            "{}",
            json!({"case":case,"event":name,"timestamp_unix_seconds":timestamp,"data":data})
        );
    }

    fn pump() {
        objc2::rc::autoreleasepool(|_| {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.025);
            NSRunLoop::mainRunLoop().runUntilDate(&until);
        });
        // Some run loops have no sources and return immediately.
        std::thread::sleep(Duration::from_millis(5));
    }

    fn app_pid(app: &NSRunningApplication) -> i32 {
        // Typed selector because the generated binding requires an optional libc feature.
        unsafe { msg_send![app, processIdentifier] }
    }

    fn running_app(pid: i32) -> Option<Retained<NSRunningApplication>> {
        unsafe {
            msg_send![NSRunningApplication::class(), runningApplicationWithProcessIdentifier: pid]
        }
    }

    fn read_identity(pid: i32) -> Result<Identity> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        if bytes != size as i32 {
            return Err(format!(
                "identity unavailable for owned fixture: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let bytes = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if bytes <= 0 {
            return Err(format!(
                "executable unavailable: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let path = std::ffi::CStr::from_bytes_until_nul(&path)?.to_str()?;
        Ok(Identity {
            pid,
            uid: info.pbi_uid,
            start_seconds: info.pbi_start_tvsec,
            start_microseconds: info.pbi_start_tvusec,
            executable: fs::canonicalize(path)?,
        })
    }

    fn foreground(app: &NSRunningApplication) -> Value {
        let frontmost = NSWorkspace::sharedWorkspace().frontmostApplication();
        json!({
            "workspace_frontmost_available": frontmost.is_some(),
            "fixture_is_frontmost": frontmost.as_ref().map(|front| &**front == app),
            "fixture_is_active": app.isActive()
        })
    }

    fn fixture_event_seen(output: &Path, case: &str, event: &str) -> bool {
        fs::read_to_string(output.join(format!("app-{case}.jsonl")))
            .ok()
            .is_some_and(|log| {
                log.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .is_some_and(|row| row["event"] == event)
                })
            })
    }

    fn identity_gate(owned: &OwnedFixture, expected: &Identity) -> Result<bool> {
        if expected.pid != owned.child.id() as i32 || owned.app.isTerminated() {
            return Ok(false);
        }
        let fresh = read_identity(owned.child.id() as i32)?;
        let fresh_app = running_app(owned.child.id() as i32);
        Ok(fresh == *expected
            && fresh.uid == unsafe { libc::getuid() }
            && app_pid(&owned.app) == owned.child.id() as i32
            && fresh_app.as_ref().is_some_and(|app| app == &owned.app)
            && owned
                .app
                .bundleIdentifier()
                .is_some_and(|id| id.to_string() == BUNDLE_ID)
            && owned
                .app
                .executableURL()
                .and_then(|url| url.path())
                .is_some_and(|path| {
                    fs::canonicalize(path.to_string()).ok().as_ref() == Some(&fresh.executable)
                }))
    }

    fn spawn_fixture(
        executable: &Path,
        output: &Path,
        case: &str,
        mode: &str,
    ) -> Result<OwnedFixture> {
        let child = Command::new(executable)
            .args([
                mode,
                output
                    .join(format!("app-{case}.jsonl"))
                    .to_str()
                    .ok_or("non-UTF8 fixture path")?,
            ])
            .spawn()?;
        let started = Instant::now();
        let pid = child.id() as i32;
        let app = loop {
            pump();
            if let Some(app) = running_app(pid).filter(|app| {
                app.isFinishedLaunching() && fixture_event_seen(output, case, "launched")
            }) {
                break app;
            }
            if started.elapsed() > Duration::from_secs(4) {
                // No terminate or signal fallback. The fixture's 23s timer is authoritative.
                event(
                    case,
                    "app_discovery_failed",
                    json!({"pid":pid,"natural_exit_wait":true}),
                );
                let mut child = child;
                while child.try_wait()?.is_none() && started.elapsed() < Duration::from_secs(27) {
                    pump();
                }
                return Err("owned fixture not discoverable through AppKit".into());
            }
        };
        let identity = read_identity(pid)?;
        let owned = OwnedFixture {
            child,
            app,
            identity,
            started,
        };
        event(
            case,
            "fixture_ready",
            json!({
                "identity":owned.identity,
                "launch_date_available":owned.app.launchDate().is_some(),
                "foreground":foreground(&owned.app),
                "identity_gate":identity_gate(&owned,&owned.identity)?
            }),
        );
        Ok(owned)
    }

    fn wait_natural(owned: &mut OwnedFixture, case: &str) -> Result<bool> {
        // Keep the fixture binaries until every owned child has naturally exited.
        loop {
            pump();
            if let Some(status) = owned.child.try_wait()? {
                event(
                    case,
                    "fixture_reaped",
                    json!({"exit_code":status.code(),"signal":status.signal(),"is_terminated":owned.app.isTerminated(),"elapsed_since_spawn_seconds":owned.started.elapsed().as_secs_f64()}),
                );
                return Ok(status.success());
            }
            if owned.started.elapsed() > Duration::from_secs(28) {
                event(
                    case,
                    "natural_exit_not_observed",
                    json!({"forced_action":false}),
                );
                return Ok(false);
            }
        }
    }

    pub fn run() -> Result<bool> {
        let args: Vec<_> = std::env::args_os().collect();
        if !(args.len() == 3 || (args.len() == 4 && args[3] == "--m3")) {
            return Err("usage: quit_probe <self-built BreeQuitFixture.app executable> <run-log-dir> [--m3]; no PID input".into());
        }
        let m3 = args.len() == 4;
        let executable = fs::canonicalize(&args[1])?;
        let output = fs::canonicalize(&args[2])?;
        if executable.file_name().and_then(|name| name.to_str()) != Some("BreeQuitFixture")
            || !executable.starts_with(&output)
            || !executable.ends_with("BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture")
        {
            return Err("fixture executable must be the dedicated self-built app inside the run-log directory".into());
        }
        event(
            "suite",
            "start",
            json!({"suite":if m3 {"m3"} else {"m0"},"scope":"own_children_only","observation_seconds":OBSERVE_SECONDS,"force_termination_available":false}),
        );
        let mut passed = true;
        let mut all_children_reaped_successfully = true;
        let mut unsaved_retention_observed = false;
        let mut unsaved_delegate_dispatch_verified = false;
        let modes = if m3 {
            vec![
                "immediate",
                "later",
                "cancel",
                "cancel-later",
                "document-cancel",
                "document-discard",
                "foreground",
            ]
        } else {
            vec!["immediate", "later", "cancel", "unsaved"]
        };
        for mode in modes {
            let mut owned = spawn_fixture(&executable, &output, mode, mode)?;
            if mode == "foreground" {
                let frozen_background = foreground(&owned.app)["fixture_is_frontmost"] == false;
                let transition_start = Instant::now();
                while !owned.app.isActive() && transition_start.elapsed() < Duration::from_secs(4) {
                    pump();
                }
                let foreground_seen =
                    owned.app.isActive() && foreground(&owned.app)["fixture_is_frontmost"] == true;
                event(
                    mode,
                    "foreground_before_execution",
                    json!({
                        "frozen_background":frozen_background,"foreground":foreground(&owned.app),
                        "identity_still_matches":identity_gate(&owned,&owned.identity)?,
                        "request_sent":false,"policy_skip":foreground_seen,
                        "atomic_frontmost_and_terminate_guarantee":false
                    }),
                );
                while owned.app.isActive() && transition_start.elapsed() < Duration::from_secs(9) {
                    pump();
                }
                let background_again = !owned.app.isActive()
                    && foreground(&owned.app)["fixture_is_frontmost"] == false;
                let transition_passed = frozen_background
                    && foreground_seen
                    && background_again
                    && fixture_event_seen(&output, mode, "fixture_self_hidden");
                event(
                    mode,
                    "foreground_transition_observation",
                    json!({
                        "background_again":background_again,"foreground":foreground(&owned.app),
                        "request_sent":false,"passed":transition_passed
                    }),
                );
                passed &= transition_passed;
                let reaped = wait_natural(&mut owned, mode)?;
                all_children_reaped_successfully &= reaped;
                passed &= reaped;
                continue;
            }
            let mut invalid = owned.identity.clone();
            invalid.start_microseconds = invalid.start_microseconds.wrapping_add(1);
            let rejected = !identity_gate(&owned, &invalid)?;
            event(
                mode,
                "stale_identity_skipped",
                json!({"same_pid":true,"changed_start_microseconds":true,"request_sent":false,"passed":rejected}),
            );
            passed &= rejected;
            let gate = identity_gate(&owned, &owned.identity)?;
            if !gate {
                event(mode, "identity_skip", json!({"request_sent":false}));
                passed = false;
                let reaped = wait_natural(&mut owned, mode)?;
                all_children_reaped_successfully &= reaped;
                passed &= reaped;
                continue;
            }
            let start = Instant::now();
            let request_accepted = owned.app.terminate();
            event(
                mode,
                "normal_quit_request",
                json!({"accepted":request_accepted,"request_sent":true,
                    "request_source":"retained_ns_running_application",
                    "immediately_terminated":owned.app.isTerminated(),"foreground":foreground(&owned.app)}),
            );
            let mut child_exit = None;
            let mut app_exit = false;
            while start.elapsed() < Duration::from_secs(OBSERVE_SECONDS) {
                pump();
                app_exit = owned.app.isTerminated();
                if child_exit.is_none() {
                    child_exit = owned.child.try_wait()?;
                }
                if app_exit && child_exit.is_some() {
                    break;
                }
            }
            let expect_exit = matches!(mode, "immediate" | "later" | "document-discard");
            let still_running = child_exit.is_none() && !app_exit;
            let reply_event = match mode {
                "immediate" => "reply_now",
                "later" => "reply_later_accept",
                "cancel" => "reply_cancel",
                "cancel-later" => "reply_later_cancel",
                "document-cancel" => "document_review_button_clicked",
                "document-discard" => "reply_now",
                _ => "reply_cancel_unsaved",
            };
            let fixture_reply_seen = fixture_event_seen(&output, mode, reply_event);
            let outcome_observed = request_accepted
                && if expect_exit {
                    app_exit && child_exit.is_some_and(|status| status.success())
                } else {
                    still_running && start.elapsed() >= Duration::from_secs(OBSERVE_SECONDS)
                };
            let review_seen = fixture_event_seen(&output, mode, "document_review_started");
            let review_button_clicked =
                fixture_event_seen(&output, mode, "document_review_button_clicked");
            let review_path_verified = match mode {
                "document-cancel" => {
                    review_seen
                        && review_button_clicked
                        && !fixture_event_seen(&output, mode, "application_should_terminate")
                }
                "document-discard" => {
                    review_seen
                        && review_button_clicked
                        && fixture_event_seen(&output, mode, "application_should_terminate")
                }
                _ => true,
            };
            let mode_passed = outcome_observed && fixture_reply_seen && review_path_verified;
            if mode == "unsaved" {
                // A registered NSDocument can take a different termination path.
                // Retention does not prove a delegate call or a save dialog.
                unsaved_retention_observed = outcome_observed
                    && fixture_event_seen(&output, mode, "unsaved_document_created");
                unsaved_delegate_dispatch_verified = fixture_reply_seen;
            } else {
                passed &= mode_passed;
            }
            event(
                mode,
                "normal_observation",
                json!({
                    "request_accepted":request_accepted,"app_is_terminated":app_exit,
                    "fixture_reply_event":reply_event,"fixture_reply_observed":fixture_reply_seen,
                    "document_review_observed":review_seen,"document_review_button_clicked":review_button_clicked,
                    "document_review_path_verified":review_path_verified,
                    "fixture_quit_apple_event_observed":fixture_event_seen(&output,mode,"fixture_quit_apple_event_received"),
                    "pid_reuse_atomicity_proven":false,
                    "child_exit_code":child_exit.and_then(|status|status.code()),
                    "still_running_after_15_seconds":still_running,
                    "elapsed_seconds":start.elapsed().as_secs_f64(),
                    "outcome_observed":outcome_observed,
                    "core_case":mode != "unsaved",
                    "passed":if mode == "unsaved" && !fixture_reply_seen {None} else {Some(mode_passed)},
                    "forced_action":false
                }),
            );
            let reaped = wait_natural(&mut owned, mode)?;
            all_children_reaped_successfully &= reaped;
            passed &= reaped;
        }
        // Same executable and bundle ID, new launch. The previous instance is never reused.
        let mut first = spawn_fixture(&executable, &output, "restart-old", "immediate")?;
        let old_identity = first.identity.clone();
        if !identity_gate(&first, &old_identity)? {
            return Err("restart fixture identity unavailable".into());
        }
        let sent = first.app.terminate();
        let reaped = wait_natural(&mut first, "restart-old")?;
        all_children_reaped_successfully &= reaped;
        passed &= sent && reaped;
        let mut replacement = spawn_fixture(&executable, &output, "restart-new", "immediate")?;
        let old_skipped = !identity_gate(&replacement, &old_identity)?;
        let distinct_app = first.app != replacement.app;
        let old_object_terminated = first.app.isTerminated();
        if m3 {
            // This request targets the frozen old AppKit instance, not a fresh PID lookup.
            let old_request_accepted = first.app.terminate();
            let observe_start = Instant::now();
            while observe_start.elapsed() < Duration::from_secs(1) {
                pump();
            }
            let replacement_untouched = !replacement.app.isTerminated()
                && replacement.child.try_wait()?.is_none()
                && !fixture_event_seen(&output, "restart-new", "application_should_terminate");
            let old_handle_passed = !old_request_accepted && replacement_untouched;
            event(
                "restart",
                "old_retained_object_request",
                json!({
                    "old_request_accepted":old_request_accepted,
                    "old_receiver_terminated":first.app.isTerminated(),
                    "replacement_untouched":replacement_untouched,
                    "same_pid_reuse_exercised":false,"passed":old_handle_passed
                }),
            );
            passed &= old_handle_passed;
        }
        event(
            "restart",
            "replacement_skipped",
            json!({
                "same_executable":old_identity.executable==replacement.identity.executable,
                "new_pid":replacement.identity.pid!=old_identity.pid,
                "distinct_running_application":distinct_app,"old_object_terminated":old_object_terminated,
                "old_plan_rejected":old_skipped,"request_sent_to_replacement":false,
                "passed":old_skipped&&distinct_app&&old_object_terminated
            }),
        );
        passed &= old_skipped && distinct_app && old_object_terminated;
        let reaped = wait_natural(&mut replacement, "restart-new")?;
        all_children_reaped_successfully &= reaped;
        passed &= reaped;
        event(
            "suite",
            "complete",
            json!({
                "core_passed":passed,
                "all_owned_children_reaped_successfully":all_children_reaped_successfully,
                // These fields describe M0's single unsaved mode, not M3's review cases.
                "unsaved_document_retention_observed":if m3 {None} else {Some(unsaved_retention_observed)},
                "unsaved_delegate_dispatch_verified":if m3 {None} else {Some(unsaved_delegate_dispatch_verified)},
                "atomic_kernel_instance_addressing_verified":false,
                "ordinary_application_support_verified":false
            }),
        );
        Ok(passed)
    }
}
