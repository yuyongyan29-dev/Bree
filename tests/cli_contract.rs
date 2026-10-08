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
        "", "status", "list", "inspect", "watch", "doctor", "clean", "license",
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

#[cfg(target_os = "macos")]
mod rules {
    use super::*;
    use bree_cli::storage::Store;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
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
                assert!(row["capabilities"].get("cleanup_enabled").is_none());
                assert_eq!(row["capabilities"]["dry_run_enabled"], true);
                assert_eq!(row["rule_storage"]["write_status"], "not_probed");
            }
        }
    }

    #[test]
    fn list_json_uses_only_categories_the_collector_assigns() {
        let store = IsolatedStore::new();
        let output = store.run(&["list", "--json"]);
        assert!(output.status.success());
        let data: Value = serde_json::from_slice(&output.stdout).unwrap();
        for item in data["processes"]
            .as_array()
            .unwrap()
            .iter()
            .chain(data["groups"].as_array().unwrap())
        {
            let category = item["category"].as_str().unwrap();
            assert!(
                ["application", "system", "unknown"].contains(&category),
                "{category}"
            );
        }
    }

    #[test]
    fn doctor_text_reports_storage_as_one_readable_line() {
        let storage_lines = |output: Output| -> Vec<String> {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<String> = text
                .lines()
                .filter(|line| line.starts_with("Storage"))
                .map(str::to_owned)
                .collect();
            for line in &lines {
                assert!(!line.contains(['{', '}', '"']), "raw JSON: {line}");
            }
            lines
        };
        let store = IsolatedStore::new();
        let missing = storage_lines(store.run(&["doctor"]));
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("(missing)"), "{}", missing[0]);
        assert!(missing[0].contains("rules default, revision 0, 0 rules"));
        assert!(missing[0].contains("journal missing"));
        assert!(missing[0].ends_with("write not probed"));
        assert!(!store.0.exists());

        store.private_root();
        store.write("state.json", "{bad");
        let invalid = storage_lines(store.run(&["doctor"]));
        assert_eq!(invalid.len(), 2);
        assert!(invalid[0].contains("(ready) · rules invalid · journal missing"));
        assert!(invalid[1].starts_with("Storage error: Rule file is corrupt"));
        assert_eq!(
            fs::read_to_string(store.0.join("state.json")).unwrap(),
            "{bad"
        );

        let relative = Command::new(env!("CARGO_BIN_EXE_bree"))
            .env("BREE_DATA_DIR", "relative-bree-data")
            .arg("doctor")
            .output()
            .unwrap();
        let unavailable = storage_lines(relative);
        assert_eq!(unavailable[0], "Storage: unavailable · write not probed");
        assert!(unavailable[1].contains("must be an absolute path"));
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
        {
            let clean = store.run(&["clean", "--dry-run", "--json"]);
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
        drop(guard);
        // A directory at the journal path is an actual I/O obstacle even for an admin account.
        fs::create_dir(store.0.join("journal.jsonl")).unwrap();
        let output = store.run(&["clean", "--dry-run", "--json"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(store.0.join("journal.jsonl").is_dir());
        assert!(!store.0.join("state.json").exists());
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

#[test]
fn removed_execution_and_history_are_rejected() {
    for args in [["clean", "--yes"].as_slice(), ["history"].as_slice()] {
        assert_eq!(bree(args).status.code(), Some(2));
    }
}
