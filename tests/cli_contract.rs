use serde_json::Value;
use std::process::{Command, Output};

fn bree(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bree"))
        .args(args)
        .output()
        .expect("run bree")
}

#[test]
fn naked_command_without_tty_never_waits_for_input() {
    let output = bree(&[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Usage:"));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn all_command_help_is_in_english() {
    for subcommand in [
        "", "status", "list", "inspect", "watch", "doctor", "clean", "history", "license",
    ] {
        let args = if subcommand.is_empty() {
            vec!["--help"]
        } else {
            vec![subcommand, "--help"]
        };
        let output = bree(&args);
        assert!(output.status.success(), "help for {subcommand}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Usage:"), "help for {subcommand}");
        assert!(
            !text.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)),
            "untranslated help for {subcommand}: {text}"
        );
        assert!(output.stderr.is_empty());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn unmatched_list_query_keeps_full_process_coverage_and_policy_without_writing_state() {
    let root = std::env::temp_dir().join(format!("bree-list-query-{}", std::process::id()));
    assert!(!root.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_bree"))
        .env("BREE_DATA_DIR", &root)
        .args([
            "list",
            "--json",
            "--search",
            "bree-no-match-673882c5-7a63",
            "--sort",
            "name",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["schema_version"], 1);
    assert!(data["groups"].as_array().unwrap().is_empty());
    assert_eq!(data["view"]["matched_groups"], 0);
    assert_eq!(data["view"]["shown_groups"], 0);
    assert_eq!(data["view"]["sort"], "name");
    assert!(!data["processes"].as_array().unwrap().is_empty());
    assert!(data["coverage"]["enumerated_processes"].as_u64().unwrap() > 0);
    assert_eq!(
        data["policy"]["entries"].as_array().unwrap().len() as u64,
        data["view"]["total_groups"].as_u64().unwrap()
    );
    assert_eq!(data["policy"]["automatic_count"], 0);
    assert!(!root.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn list_search_sort_and_limit_are_applied_in_that_order() {
    let output = bree(&[
        "list", "--json", "--search", "  BrEe  ", "--sort", "name", "--limit", "1",
    ]);
    assert!(output.status.success());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["view"]["search"], "BrEe");
    assert_eq!(data["view"]["sort"], "name");
    let groups = data["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(data["view"]["shown_groups"], 1);
    assert!(data["view"]["matched_groups"].as_u64().unwrap() >= 1);
    for group in groups {
        let members = group["process_ids"].as_array().unwrap();
        assert!(
            group["name"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("bree")
                || data["processes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| members.contains(&p["id"])
                        && p["name"].as_str().unwrap().to_lowercase().contains("bree"))
        );
    }
    let all_names = bree(&["list", "--json", "--sort", "name", "--limit", "20"]);
    assert!(all_names.status.success());
    let names: Value = serde_json::from_slice(&all_names.stdout).unwrap();
    assert_eq!(
        names["view"]["total_groups"],
        names["view"]["matched_groups"]
    );
    let groups = names["groups"].as_array().unwrap();
    assert!(
        groups
            .windows(2)
            .all(|pair| pair[0]["name"].as_str().unwrap().to_lowercase()
                <= pair[1]["name"].as_str().unwrap().to_lowercase())
    );
    let zero = bree(&["list", "--json", "--search", "bree", "--limit", "0"]);
    assert!(zero.status.success());
    let zero: Value = serde_json::from_slice(&zero.stdout).unwrap();
    assert!(zero["groups"].as_array().unwrap().is_empty());
    assert!(zero["view"]["matched_groups"].as_u64().unwrap() >= 1);
    assert_eq!(zero["view"]["shown_groups"], 0);
}

#[test]
fn invalid_search_and_sort_are_rejected_before_sampling() {
    for (option, value) in [
        ("--search", "unsafe\ninput".into()),
        ("--search", "中".repeat(129)),
        ("--sort", "cpu".into()),
    ] {
        let output = bree(&["list", option, &value, "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn numeric_list_query_returns_only_the_group_containing_the_exact_pid() {
    let pid = std::process::id().to_string();
    let output = bree(&["list", "--json", "--search", &pid]);
    assert!(output.status.success());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    let groups = data["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let process = data["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["identity"]["pid"].as_u64() == Some(std::process::id() as u64))
        .unwrap();
    assert!(
        groups[0]["process_ids"]
            .as_array()
            .unwrap()
            .contains(&process["id"])
    );
    assert_eq!(data["view"]["matched_groups"], 1);
}

#[test]
fn installed_program_carries_project_and_dependency_notices_without_storage() {
    let directory = std::env::temp_dir().join(format!("bree-license-test-{}", std::process::id()));
    assert!(!directory.exists());
    for third_party in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bree"));
        command
            .env("BREE_DATA_DIR", &directory)
            .args(["license", "--json"]);
        if third_party {
            command.arg("--third-party");
        }
        let result = command.output().unwrap();
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["license"], "GPL-3.0-only");
        assert_eq!(value["third_party"], third_party);
        let text = value["text"].as_str().unwrap();
        assert!(text.contains(if third_party {
            "Unicode"
        } else {
            "GNU GENERAL PUBLIC LICENSE"
        }));
    }
    assert!(!directory.exists());
}

#[test]
fn noninteractive_cleanup_requires_consent_and_never_supports_force() {
    for args in [
        vec!["clean"],
        vec!["clean", "--force", "--yes"],
        vec!["clean", "--dry-run", "--yes"],
    ] {
        let output = bree(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[cfg(target_os = "macos")]
mod rules {
    use super::*;
    use bree_cli::storage::Store;
    use std::{
        fs,
        os::fd::AsRawFd,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Child, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct IsolatedStore(PathBuf);
    impl IsolatedStore {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "bree-command-rules-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )))
        }
        fn run(&self, args: &[&str]) -> Output {
            Command::new(env!("CARGO_BIN_EXE_bree"))
                .env("BREE_DATA_DIR", &self.0)
                .args(args)
                .output()
                .unwrap()
        }
        fn private_root(&self) {
            fs::create_dir(&self.0).unwrap();
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn write(&self, name: &str, contents: &str) {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    impl Drop for IsolatedStore {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    // Any assertion failure still reaps only the batch created by this test.
    struct OwnedBatch(Option<Child>);
    impl OwnedBatch {
        fn spawn(store: &IsolatedStore) -> Self {
            Self(Some(
                Command::new(env!("CARGO_BIN_EXE_bree"))
                    .env("BREE_DATA_DIR", &store.0)
                    .args(["clean", "--yes", "--json"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            ))
        }
        fn cancel_while_lane_owned(&mut self, lane: &fs::File) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // The lane is acquired after CLI signal registration. A failed
                // probe proves this exact child is in Session::start, while the
                // parent's journal lock prevents its started record completing.
                let locked =
                    unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if locked != 0 {
                    assert_eq!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::WouldBlock
                    );
                    break;
                }
                assert_eq!(unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_UN) }, 0);
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "owned batch exited before entering the execution lane"
                );
                assert!(
                    Instant::now() < deadline,
                    "owned batch never acquired its lane"
                );
                thread::sleep(Duration::from_millis(2));
            }
            let pid = self.0.as_ref().unwrap().id() as i32;
            assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
        }
        fn output_within(&mut self, timeout: Duration) -> Output {
            let deadline = Instant::now() + timeout;
            loop {
                if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled owned batch did not finish within {timeout:?}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            self.0.take().unwrap().wait_with_output().unwrap()
        }
    }
    impl Drop for OwnedBatch {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                match child.try_wait() {
                    Ok(Some(_)) => {}
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
        }
    }

    #[test]
    fn reading_with_no_config_creates_nothing_and_explains_zero_candidates() {
        let store = IsolatedStore::new();
        for args in [["list", "--json"], ["doctor", "--json"]] {
            let output = store.run(&args);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let row: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(row["schema_version"], 1);
            assert_eq!(row["policy_state_valid"], true);
            assert!(
                !store.0.exists(),
                "read-only commands must not initialize storage"
            );
            if args[0] == "list" {
                assert_eq!(row["policy"]["automatic_count"], 0);
                assert_eq!(row["policy"]["read_only"], true);
            } else {
                assert_eq!(row["capabilities"]["cleanup_enabled"], false);
                assert_eq!(row["capabilities"]["dry_run_enabled"], true);
                assert_eq!(row["rule_storage"]["write_status"], "not_probed");
            }
        }
    }

    #[test]
    fn dry_run_freezes_read_only_plan_and_writes_only_summary() {
        let store = IsolatedStore::new();
        let output = store.run(&["clean", "--dry-run", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["read_only"], true);
        assert_eq!(plan["automatic_count"], 0);
        assert_eq!(plan["rule_revision"], 0);
        assert!(
            plan["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["disposition"] != "automatic")
        );
        assert!(!store.0.join("state.json").exists());
        let journal = fs::read_to_string(store.0.join("journal.jsonl")).unwrap();
        let records: Vec<Value> = journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["event"], "dry_run_prepared");
        assert_eq!(records[0]["data"]["plan_id"], plan["plan_id"]);
        assert!(records[0]["data"].get("entries").is_none());
        assert!(!journal.contains("executable_path"));
        assert!(!journal.contains("bundle_path"));
        assert_eq!(
            fs::metadata(store.0.join("journal.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupt_rules_disable_preview_but_keep_reading_and_preserve_original() {
        let store = IsolatedStore::new();
        store.private_root();
        store.write("state.json", "{broken");
        let list = store.run(&["list", "--json"]);
        assert!(list.status.success());
        let row: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(row["policy_state_valid"], false);
        assert_eq!(row["policy"]["automatic_count"], 0);
        assert!(
            row["policy"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["disposition"] == "protected")
        );
        for args in [
            ["clean", "--dry-run", "--json"],
            ["clean", "--yes", "--json"],
        ] {
            let clean = store.run(&args);
            assert_eq!(clean.status.code(), Some(1));
            let error: Value = serde_json::from_slice(&clean.stdout).unwrap();
            assert_eq!(error["error"]["code"], "runtime_error");
        }
        assert_eq!(
            fs::read_to_string(store.0.join("state.json")).unwrap(),
            "{broken"
        );
        assert!(!store.0.join("journal.jsonl").exists());
    }

    #[test]
    fn execution_lock_and_failed_journal_block_new_preview() {
        let store = IsolatedStore::new();
        let lane = Store::at(store.0.clone());
        let guard = lane.execution_lock().unwrap();
        let output = store.run(&["clean", "--dry-run", "--json"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cleanup or dry run is in progress")
        );
        let batch = store.run(&["clean", "--yes", "--json"]);
        assert_eq!(batch.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&batch.stderr).contains("cleanup or dry run is in progress")
        );
        drop(guard);
        // A directory at the journal path is an actual I/O obstacle even for an admin account.
        fs::create_dir(store.0.join("journal.jsonl")).unwrap();
        let output = store.run(&["clean", "--dry-run", "--json"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(store.0.join("journal.jsonl").is_dir());
        assert!(!store.0.join("state.json").exists());
    }

    #[test]
    fn zero_target_batch_records_result_then_history_reads_without_replay() {
        let store = IsolatedStore::new();
        let history = store.run(&["history", "--json"]);
        assert!(history.status.success());
        let empty: Value = serde_json::from_slice(&history.stdout).unwrap();
        assert_eq!(empty["records"].as_array().unwrap().len(), 0);
        assert!(!store.0.exists());
        let batch = store.run(&["clean", "--yes", "--json"]);
        assert!(
            batch.status.success(),
            "{}",
            String::from_utf8_lossy(&batch.stderr)
        );
        let result: Value = serde_json::from_slice(&batch.stdout).unwrap();
        assert_eq!(result["schema_version"], 1);
        assert_eq!(result["targets"].as_array().unwrap().len(), 0);
        assert!(result["resource_after"].is_null());
        assert!(
            result["resource_observation"]
                .as_str()
                .unwrap()
                .contains("No termination requests were sent")
        );
        let before = fs::read(store.0.join("journal.jsonl")).unwrap();
        let history = store.run(&["history", "--json"]);
        assert!(history.status.success());
        let history: Value = serde_json::from_slice(&history.stdout).unwrap();
        assert_eq!(history["replay_enabled"], false);
        assert_eq!(history["records"][0]["status"], "finished");
        assert_eq!(history["records"][0]["result"]["run_id"], result["run_id"]);
        assert_eq!(fs::read(store.0.join("journal.jsonl")).unwrap(), before);
        let doctor = store.run(&["doctor", "--json"]);
        let doctor: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        assert_eq!(doctor["capabilities"]["cleanup_enabled"], false);
        assert_eq!(doctor["capabilities"]["cleanup_session_enabled"], true);
        assert_eq!(doctor["capabilities"]["history_enabled"], true);
    }

    #[test]
    fn cancelled_batch_during_started_record_wait_finishes_without_sending_or_replay() {
        let store = IsolatedStore::new();
        let initialized = store.run(&["clean", "--yes", "--json"]);
        assert!(
            initialized.status.success(),
            "{}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        let initial: Value = serde_json::from_slice(&initialized.stdout).unwrap();
        assert!(initial["targets"].as_array().unwrap().is_empty());
        let journal_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.0.join("journal.lock"))
            .unwrap();
        // The lock is held only by this parent's owned descriptor, not a fixture App.
        assert_eq!(
            unsafe { libc::flock(journal_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let lane = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.0.join("execution.lock"))
            .unwrap();
        let mut child = OwnedBatch::spawn(&store);
        child.cancel_while_lane_owned(&lane);
        // Release within the 250 ms journal-lock budget, after the cancellation
        // handler has had an opportunity to set its cooperative flag.
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            unsafe { libc::flock(journal_lock.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        let output = child.output_within(Duration::from_secs(3));
        assert_eq!(
            output.status.code(),
            Some(130),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["cancelled"], true);
        assert!(
            result["targets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|target| target["request_sent"] == false)
        );
        assert!(result["resource_after"].is_null());
        assert!(result["errors"].as_array().unwrap().is_empty());
        let before_history = fs::read(store.0.join("journal.jsonl")).unwrap();
        let history = store.run(&["history", "--json"]);
        assert!(history.status.success());
        let history: Value = serde_json::from_slice(&history.stdout).unwrap();
        assert_eq!(history["replay_enabled"], false);
        assert_eq!(history["records"][0]["status"], "cancelled");
        assert_eq!(history["records"][0]["result"]["run_id"], result["run_id"]);
        assert_eq!(history["records"].as_array().unwrap().len(), 2);
        assert_eq!(
            fs::read(store.0.join("journal.jsonl")).unwrap(),
            before_history
        );
        let retained: Vec<Value> = String::from_utf8(before_history)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            retained.len(),
            4,
            "both zero-target runs have start and finish only"
        );
        assert!(retained.iter().all(|row| {
            row["event"] == "cleanup_started" || row["event"] == "cleanup_finished"
        }));
        assert_eq!(
            unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "cancelled batch must release its execution lane"
        );
        assert_eq!(unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(!store.0.join("state.json").exists());
    }

    #[test]
    fn cancelled_batch_when_started_record_lock_times_out_reports_cancelled_error() {
        let store = IsolatedStore::new();
        let initialized = store.run(&["clean", "--yes", "--json"]);
        assert!(
            initialized.status.success(),
            "{}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        let before = fs::read(store.0.join("journal.jsonl")).unwrap();
        let journal_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.0.join("journal.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(journal_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let lane = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.0.join("execution.lock"))
            .unwrap();
        let mut child = OwnedBatch::spawn(&store);
        child.cancel_while_lane_owned(&lane);
        // Keep the journal lock held through the entire 250 ms budget. There
        // cannot be a durable started record or a fabricated finished result.
        let output = child.output_within(Duration::from_secs(3));
        assert_eq!(
            output.status.code(),
            Some(130),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "cancelled");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("journal.lock")
        );
        assert!(error.get("run_id").is_none());
        assert!(error.get("targets").is_none());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Command cancelled"));
        assert_eq!(fs::read(store.0.join("journal.jsonl")).unwrap(), before);
        assert_eq!(
            unsafe { libc::flock(journal_lock.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        let history = store.run(&["history", "--json"]);
        assert!(history.status.success());
        let history: Value = serde_json::from_slice(&history.stdout).unwrap();
        assert_eq!(history["replay_enabled"], false);
        assert_eq!(history["records"].as_array().unwrap().len(), 1);
        assert_eq!(history["records"][0]["status"], "finished");
        assert_eq!(fs::read(store.0.join("journal.jsonl")).unwrap(), before);
        assert_eq!(
            unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "a cancelled initialization error must release its execution lane"
        );
        assert_eq!(unsafe { libc::flock(lane.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(!store.0.join("state.json").exists());
    }

    #[test]
    fn incomplete_history_is_unknown_and_corrupt_history_is_not_hidden_by_limit() {
        let store = IsolatedStore::new();
        Store::at(store.0.clone())
            .append_record(
                "target_request_prepared",
                serde_json::json!({"run_id":"test-run"}),
            )
            .unwrap();
        let before = fs::read(store.0.join("journal.jsonl")).unwrap();
        let history = store.run(&["history", "--json"]);
        assert!(history.status.success());
        let history: Value = serde_json::from_slice(&history.stdout).unwrap();
        assert_eq!(history["records"][0]["status"], "unfinished");
        assert!(history["records"][0]["result"].is_null());
        assert_eq!(fs::read(store.0.join("journal.jsonl")).unwrap(), before);
        store.write("journal.jsonl", "{broken\n");
        let history = store.run(&["history", "--json", "--limit", "0"]);
        assert_eq!(history.status.code(), Some(1));
        assert_eq!(
            fs::read_to_string(store.0.join("journal.jsonl")).unwrap(),
            "{broken\n"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn real_snapshot_preserves_missing_values_and_redacts_paths() {
    let output = bree(&["list", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["schema_version"], 1);
    let processes = snapshot["processes"].as_array().unwrap();
    assert!(!processes.is_empty());
    for process in processes {
        assert!(process["executable_path"].is_null());
        assert_eq!(process["quit_supported"], false);
        let metric = &process["memory_bytes"];
        if metric["status"] != "ok" {
            assert!(metric["value"].is_null());
        }
        assert!(
            process["cpu_one_core_percent"]["value"].is_null(),
            "first sample is a baseline"
        );
    }
    let total = snapshot["system"]["total_bytes"]["value"].as_u64().unwrap();
    assert!(total > 0);
    let mut expected = None;
    for process in processes {
        if process["identity"]["pid"].as_u64() == Some(std::process::id() as u64) {
            expected = process["id"].as_str().map(str::to_owned);
        }
    }
    let id = expected.expect("test runner visible in fresh snapshot");
    let detail = bree(&["inspect", &id, "--json"]);
    assert!(
        detail.status.success(),
        "{}",
        String::from_utf8_lossy(&detail.stderr)
    );
    let detail: Value = serde_json::from_slice(&detail.stdout).unwrap();
    assert_eq!(
        detail["processes"][0]["identity"]["pid"],
        std::process::id()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn watch_outputs_jsonl_and_builds_a_cpu_baseline() {
    let output = bree(&["watch", "--json", "--count", "2", "--interval", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let rows: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows[1]["sampled_at_unix_ms"].as_u64() > rows[0]["sampled_at_unix_ms"].as_u64());
    assert!(
        rows[1]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["cpu_one_core_percent"]["status"] == "ok")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn stale_inspect_fails_as_structured_error() {
    let output = bree(&["inspect", "p:invalid:1:1:0", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["schema_version"], 1);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("identity")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn cancelled_json_watch_exits_even_when_its_pipe_is_not_read() {
    use std::{
        io::{self, Read},
        os::fd::AsRawFd,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_bree"))
        .args(["watch", "--json", "--interval", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // One byte proves startup and signal registration have finished. Then stop
    // reading the private pipe while the rest of the real snapshot fills it.
    let fd = child.stdout.as_ref().unwrap().as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let ready = loop {
        let mut byte = [0_u8];
        match child.stdout.as_mut().unwrap().read(&mut byte) {
            Ok(1) => break byte == [b'{'],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            _ => break false,
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    if !ready {
        drop(child.stdout.take());
        let _ = child.kill();
        let _ = child.wait();
        panic!("owned watch did not begin its JSON result");
    }
    thread::sleep(Duration::from_millis(50));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(130));
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Bounded failure cleanup only for the exact process created above.
    drop(child.stdout.take());
    let _ = child.kill();
    let _ = child.wait();
    panic!("cancelled watch remained blocked on its private output pipe");
}
