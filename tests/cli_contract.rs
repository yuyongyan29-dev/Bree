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

fn assert_keys(value: &Value, expected: &[&str]) {
    use std::collections::BTreeSet;
    let actual: BTreeSet<_> = value
        .as_object()
        .expect("JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(actual, expected.iter().copied().collect());
}

fn assert_metric(metric: &Value) {
    assert_keys(metric, &["value", "status", "reason"]);
    let status = metric["status"].as_str().expect("metric status");
    assert!(["ok", "denied", "unsupported", "exited", "stale", "unknown"].contains(&status));
    if status == "ok" {
        assert!(!metric["value"].is_null());
        assert!(metric["reason"].is_null());
    } else {
        assert!(metric["value"].is_null());
        assert!(
            !metric["reason"]
                .as_str()
                .expect("unavailable reason")
                .is_empty()
        );
    }
}

// Keep these exact sets in sync with both user guides' Output contract tables.
// Additions fail too, so extending schema 2 requires an explicit contract review.
fn assert_json_contract(value: &Value, command: &str) {
    let keys: &[&str] = match command {
        "status" => &[
            "schema_version",
            "sampled_at_unix_ms",
            "collected_in_ms",
            "system",
            "coverage",
            "diagnostics",
        ],
        "list" => &[
            "schema_version",
            "sampled_at_unix_ms",
            "collected_in_ms",
            "system",
            "processes",
            "groups",
            "coverage",
            "diagnostics",
            "view",
        ],
        "inspect" => &["schema_version", "sampled_at_unix_ms", "group", "processes"],
        "watch" => &[
            "schema_version",
            "sampled_at_unix_ms",
            "collected_in_ms",
            "system",
            "processes",
            "groups",
            "coverage",
            "diagnostics",
        ],
        "doctor" => &[
            "schema_version",
            "version",
            "platform",
            "architecture",
            "sampled_at_unix_ms",
            "coverage",
            "diagnostics",
            "capabilities",
            "notes",
        ],
        "license" => &["schema_version", "license", "third_party", "text"],
        "error" => &["schema_version", "error"],
        _ => panic!("missing JSON contract for {command}"),
    };
    assert_keys(value, keys);
    assert_eq!(value["schema_version"], 2);
    if command == "error" {
        assert_keys(&value["error"], &["code", "message"]);
        assert_eq!(value["error"]["code"], "runtime_error");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
    }
    if let Some(system) = value.get("system") {
        for field in [
            "total_bytes",
            "used_bytes",
            "compressed_bytes",
            "swap_used_bytes",
            "cached_bytes",
            "pressure",
        ] {
            assert_metric(&system[field]);
        }
    }
    if let Some(processes) = value.get("processes") {
        for process in processes.as_array().unwrap() {
            assert_metric(&process["memory_bytes"]);
            assert_metric(&process["cpu_one_core_percent"]);
        }
    }
    if let Some(groups) = value.get("groups") {
        for group in groups.as_array().unwrap() {
            assert_metric(&group["memory_bytes"]);
        }
    }
    if let Some(group) = value.get("group").filter(|group| !group.is_null()) {
        assert_metric(&group["memory_bytes"]);
    }
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
        assert_json_contract(&value, "license");
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
    for row in &rows {
        assert_json_contract(row, "watch");
    }
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
    // Capture the ID of our own sampling child, which has exited before inspect.
    let home = TestHome::new();
    let child = home
        .command()
        .args(["list", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let sample = child.wait_with_output().unwrap();
    assert!(sample.status.success());
    let sample: Value = serde_json::from_slice(&sample.stdout).unwrap();
    let expired = sample["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["identity"]["pid"] == pid)
        .unwrap()["id"]
        .as_str()
        .unwrap();
    for id in [expired, "p:invalid:1:1:0"] {
        let output = home.run(&["inspect", id, "--json"]);
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_json_contract(&error, "error");
        assert_eq!(error["schema_version"], 2);
        assert_eq!(error["error"]["code"], "runtime_error");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("identity")
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn inspect_accepts_pid_and_group_short_id_with_current_members() {
    let pid = std::process::id().to_string();
    let output = bree(&["inspect", &pid, "--json"]);
    assert!(output.status.success());
    let detail: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(detail["group"].is_null());
    assert_eq!(detail["processes"].as_array().unwrap().len(), 1);
    assert_eq!(
        detail["processes"][0]["identity"]["pid"],
        std::process::id()
    );

    let sample = bree(&["list", "--json", "--search", &pid, "--limit", "1"]);
    assert!(sample.status.success());
    let sample: Value = serde_json::from_slice(&sample.stdout).unwrap();
    let group = &sample["groups"][0];
    let short = group["short_id"].as_str().unwrap();
    assert!(short.len() >= 8 && short.bytes().all(|c| c.is_ascii_hexdigit()));
    let output = bree(&["inspect", short, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let detail: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(detail["schema_version"], 2);
    assert!(detail["sampled_at_unix_ms"].is_u64());
    assert_eq!(detail["group"]["id"], group["id"]);
    assert_eq!(detail["group"]["short_id"], short);
    assert!(
        detail["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["identity"]["pid"] == std::process::id())
    );
    let output = bree(&["inspect", short]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let summary = text.lines().next().unwrap();
    assert!(summary.contains("instances · Total") && summary.contains(short));
    assert!(!text.contains("Object ID:") && !text.contains("Unix ms"));
}

#[cfg(target_os = "macos")]
#[test]
fn missing_pid_and_short_id_return_structured_runtime_errors() {
    for id in ["4294967295", "ffffffffffffffffffffffffffffffff"] {
        let output = bree(&["inspect", id, "--json"]);
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_json_contract(&error, "error");
        assert_eq!(error["schema_version"], 2);
        assert_eq!(error["error"]["code"], "runtime_error");
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn text_uses_local_clock_and_reserves_definitions_for_doctor() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let home = TestHome::new();
    let epoch = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    for (timezone, offset) in [("UTC0", 0), ("EST5", -5 * 3600)] {
        let start = epoch();
        let output = home
            .command()
            .env("TZ", timezone)
            .arg("status")
            .output()
            .unwrap();
        let end = epoch();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("Definition:") && !text.contains("Unix ms"));
        assert!(
            (start..=end).any(|seconds| {
                let day = (seconds as i64 + offset).rem_euclid(86400);
                text.contains(&format!(
                    "Sampled at: {:02}:{:02}:{:02}",
                    day / 3600,
                    day / 60 % 60,
                    day % 60
                ))
            }),
            "{text}"
        );
    }
    let pid = std::process::id().to_string();
    for args in [
        vec!["list", "--limit", "1"],
        vec!["inspect", &pid],
        vec!["watch", "--count", "2", "--interval", "1"],
    ] {
        let output = home.run(&args);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("Definition:") && !text.contains("Unix ms"));
        let times: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("Sampled at: "))
            .collect();
        assert_eq!(times.len(), if args[0] == "watch" { 2 } else { 1 });
        for time in times {
            let clock = &time[..8];
            assert_eq!(clock.as_bytes()[2], b':');
            assert_eq!(clock.as_bytes()[5], b':');
            assert!(
                clock
                    .bytes()
                    .enumerate()
                    .all(|(i, c)| [2, 5].contains(&i) || c.is_ascii_digit())
            );
        }
    }
    let doctor = home.run(&["doctor", "--json"]);
    assert!(doctor.status.success());
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let text = String::from_utf8(home.run(&["doctor"]).stdout).unwrap();
    for note in report["notes"].as_array().unwrap() {
        assert!(text.contains(note.as_str().unwrap()));
    }
    assert!(text.contains("Definition: Used ="));
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
    assert_json_contract(&sample, "list");
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
    let pid = std::process::id().to_string();
    let short = sample["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| {
            group["process_ids"]
                .as_array()
                .unwrap()
                .contains(&own["id"])
        })
        .unwrap()["short_id"]
        .as_str()
        .unwrap();
    for args in [
        vec!["status"],
        vec!["status", "--json"],
        vec!["list"],
        vec!["list", "--json"],
        vec!["inspect", id],
        vec!["inspect", id, "--json"],
        vec!["inspect", &pid],
        vec!["inspect", &pid, "--json"],
        vec!["inspect", short],
        vec!["inspect", short, "--json"],
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
        if args.contains(&"--json") {
            assert!(output.stderr.is_empty());
            // JSONL needs one complete contract per line, not one for the stream.
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let value: Value = serde_json::from_str(line).unwrap();
                assert_json_contract(&value, args[0]);
            }
            assert!(!output.stdout.is_empty());
        }
        if args == ["doctor", "--json"] {
            let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                doctor["capabilities"],
                serde_json::json!({"read_only":true, "ai_attribution_enabled":true, "background_service":false})
            );
        }
    }
}
