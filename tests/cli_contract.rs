use serde_json::Value;
use std::process::{Command, Output};

struct TestHome(std::path::PathBuf);

impl TestHome {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "bree-command-home-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bree"));
        command
            .env("HOME", &self.0)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env_remove("COLORFGBG");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().expect("run bree");
        assert!(
            std::fs::read_dir(&self.0).unwrap().next().is_none(),
            "Bree wrote to HOME after {args:?}"
        );
        output
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bree(args: &[&str]) -> Output {
    TestHome::new().run(args)
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
        "", "status", "list", "inspect", "watch", "doctor", "license",
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
fn unmatched_list_query_keeps_full_process_coverage() {
    let output = bree(&[
        "list",
        "--json",
        "--search",
        "bree-no-match-673882c5-7a63",
        "--sort",
        "name",
        "--limit",
        "1",
    ]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["schema_version"], 2);
    assert!(data["groups"].as_array().unwrap().is_empty());
    assert_eq!(data["view"]["matched_groups"], 0);
    assert_eq!(data["view"]["shown_groups"], 0);
    assert_eq!(data["view"]["sort"], "name");
    assert!(!data["processes"].as_array().unwrap().is_empty());
    assert!(data["coverage"]["enumerated_processes"].as_u64().unwrap() > 0);
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
fn installed_program_carries_project_and_dependency_notices_offline() {
    for third_party in [false, true] {
        let home = TestHome::new();
        let mut command = home.command();
        command.args(["license", "--json"]);
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
}

#[cfg(target_os = "macos")]
#[test]
fn list_json_uses_only_categories_the_collector_assigns() {
    let output = bree(&["list", "--json"]);
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
    assert_eq!(snapshot["schema_version"], 2);
    let processes = snapshot["processes"].as_array().unwrap();
    assert!(!processes.is_empty());
    for process in processes {
        assert!(process["executable_path"].is_null());
        let metric = &process["memory_bytes"];
        for metric in [&process["memory_bytes"], &process["cpu_one_core_percent"]] {
            let keys: Vec<_> = metric
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, ["reason", "status", "value"]);
        }
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
    assert_eq!(error["schema_version"], 2);
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
    let home = TestHome::new();
    let mut child = home
        .command()
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
fn removed_commands_are_rejected() {
    for args in [
        ["clean"].as_slice(),
        ["clean", "--yes"].as_slice(),
        ["clean", "--dry-run"].as_slice(),
        ["history"].as_slice(),
    ] {
        assert_eq!(bree(args).status.code(), Some(2));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn all_commands_leave_an_empty_home_unchanged() {
    let home = TestHome::new();
    let sample = home.run(&["list", "--json"]);
    assert!(sample.status.success());
    let sample: Value = serde_json::from_slice(&sample.stdout).unwrap();
    assert_eq!(sample["schema_version"], 2);
    let keys: Vec<_> = sample
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "collected_in_ms",
            "coverage",
            "diagnostics",
            "groups",
            "processes",
            "sampled_at_unix_ms",
            "schema_version",
            "system",
            "view"
        ]
    );
    let processes = sample["processes"].as_array().unwrap();
    let own = processes
        .iter()
        .find(|p| p["identity"]["pid"].as_u64() == Some(std::process::id() as u64))
        .unwrap();
    let keys: Vec<_> = own
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "attribution",
            "category",
            "cpu_one_core_percent",
            "development_label",
            "executable_path",
            "id",
            "identity",
            "memory_bytes",
            "metric_kind",
            "name",
            "parent_pid",
            "uid"
        ]
    );
    let id = own["id"].as_str().unwrap();
    for args in [
        vec!["status"],
        vec!["status", "--json"],
        vec!["list"],
        vec!["list", "--json"],
        vec!["inspect", id],
        vec!["inspect", id, "--json"],
        vec!["watch", "--count", "2", "--interval", "1"],
        vec!["watch", "--json", "--count", "2", "--interval", "1"],
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["license"],
        vec!["license", "--json"],
        vec!["license", "--third-party"],
        vec!["license", "--third-party", "--json"],
    ] {
        let output = home.run(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args == ["doctor", "--json"] {
            let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                doctor["capabilities"],
                serde_json::json!({"read_only":true, "ai_attribution_enabled":true, "background_service":false})
            );
        }
    }
}
