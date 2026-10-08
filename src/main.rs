use bree_cli::{
    attribution::label_process,
    collect::Collector,
    model::{SCHEMA_VERSION, Snapshot, safe_text},
    output,
    query::{GroupSort, compare_groups, group_matches, validate_search},
    tui,
};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "bree",
    version,
    about = "Understand memory usage on your Mac",
    long_about = "Inspect and export local memory usage without saving state. Run bree in an interactive terminal to open the menu. Bree does not quit applications."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show a single system memory snapshot
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List memory usage by verified application ownership; retain unknown owners
    List {
        #[arg(long)]
        json: bool,
        /// Limit displayed groups; JSON retains the full process table and coverage
        #[arg(long, value_parser = clap::value_parser!(usize))]
        limit: Option<usize>,
        /// Match names or bundle IDs without case sensitivity; pure digits match an exact PID
        #[arg(long, default_value = "", value_parser = validate_search)]
        search: String,
        /// Order displayed groups; unavailable memory stays after measured values
        #[arg(long, value_enum, default_value = "memory")]
        sort: GroupSort,
    },
    /// Inspect an object ID from list using a fresh sample of the current instance
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Watch in the foreground; output text or JSONL when piped
    Watch {
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
        interval: u64,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        count: Option<u64>,
    },
    /// Check sampling coverage and capabilities without requesting permissions
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show the GPL-3.0 license or bundled third-party notices offline
    License {
        #[arg(long)]
        third_party: bool,
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    if cli.command.is_none() && !tty {
        let code = match output::write_text(
            "bree · Alpha\nUsage: bree status | list | inspect <id> | watch | doctor | license\nRun bree in an interactive terminal to open the menu.",
        ) {
            Ok(()) => 0,
            Err(_) => 1,
        };
        std::process::exit(code);
    }
    let interactive = cli.command.is_none()
        || matches!(
            cli.command,
            Some(Command::Watch {
                json: false,
                count: None,
                ..
            })
        ) && tty;
    let wants_json = match &cli.command {
        Some(Command::Status { json })
        | Some(Command::List { json, .. })
        | Some(Command::Inspect { json, .. })
        | Some(Command::Watch { json, .. })
        | Some(Command::Doctor { json })
        | Some(Command::License { json, .. }) => *json,
        None => false,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    if !interactive
        && let Err(error) = ctrlc::set_handler(|| {
            // A blocked output consumer must not prevent cancellation.
            std::process::exit(130);
        })
    {
        let _ = writeln!(
            io::stderr(),
            "bree: Cannot register the cancellation handler: {error}"
        );
        std::process::exit(1);
    }
    let result = execute(cli.command, tty, &cancelled);
    let code = match result {
        Ok(code) => {
            if cancelled.load(Ordering::Relaxed) {
                130
            } else {
                code
            }
        }
        Err(error) => {
            let was_cancelled = cancelled.load(Ordering::Acquire);
            let message = if was_cancelled {
                format!("Command cancelled; {error}")
            } else {
                error
            };
            if wants_json {
                let _ = output::write_json(
                    &json!({"schema_version":SCHEMA_VERSION,"error":{"code":if was_cancelled {"cancelled"} else {"runtime_error"},"message":bree_cli::model::safe_text(&message)}}),
                );
            }
            // A terminal disconnect can also make stderr unwritable. Preserve the
            // command's error code instead of panicking while reporting its error.
            let _ = writeln!(
                io::stderr(),
                "bree: {}",
                bree_cli::model::safe_text(&message)
            );
            if was_cancelled { 130 } else { 1 }
        }
    };
    std::process::exit(code);
}

fn execute(
    command: Option<Command>,
    tty: bool,
    cancelled: &Arc<AtomicBool>,
) -> Result<i32, String> {
    if command.is_none() {
        let was_cancelled = tui::run(false, Duration::from_secs(2))?;
        cancelled.store(was_cancelled, Ordering::Relaxed);
        return Ok(0);
    }
    if let Some(Command::License { third_party, json }) = &command {
        let text = if *third_party {
            include_str!("../THIRD-PARTY-NOTICES.txt")
        } else {
            include_str!("../LICENSE")
        };
        if *json {
            output::write_json(
                &json!({"schema_version":SCHEMA_VERSION,"license":"GPL-3.0-only","third_party":third_party,"text":text}),
            )?;
        } else {
            output::write_text(text)?;
        }
        return Ok(0);
    }
    if let Some(Command::Watch {
        interval,
        json: false,
        count: None,
    }) = &command
        && tty
    {
        let was_cancelled = tui::run(true, Duration::from_secs(*interval))?;
        cancelled.store(was_cancelled, Ordering::Relaxed);
        return Ok(0);
    }
    let mut collector = Collector::new()?;
    if let Some(Command::Watch {
        interval,
        json,
        count,
    }) = command
    {
        let mut n = 0;
        while !cancelled.load(Ordering::Relaxed) {
            let snapshot = collector.snapshot()?;
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            if json {
                output::write_json(&snapshot_json(&snapshot))?;
            } else {
                output::write_text(&output::status_text(&snapshot))?;
            }
            n += 1;
            if count.is_some_and(|limit| n >= limit) {
                break;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(interval);
            while std::time::Instant::now() < deadline && !cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        return Ok(0);
    }
    let snapshot = collector.snapshot()?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(130);
    }
    match command.expect("interactive handled above") {
        Command::Status { json: true } => output::write_json(
            &json!({"schema_version":SCHEMA_VERSION,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"collected_in_ms":snapshot.collected_in_ms,"system":snapshot.system,"coverage":snapshot.coverage,"diagnostics":snapshot.diagnostics}),
        ),
        Command::Status { json: false } => output::write_text(&output::status_text(&snapshot)),
        Command::List {
            json,
            limit,
            search,
            sort,
        } => {
            // This display view keeps the complete process sample and coverage.
            // Filtering cannot change ownership.
            let mut display = snapshot.clone();
            display.groups.retain(|group| group_matches(&snapshot, group, &search));
            display.groups.sort_by(|a, b| compare_groups(a, b, sort));
            let matched = display.groups.len();
            let shown = limit.unwrap_or(matched).min(matched);
            if json {
                display.groups.truncate(shown);
                let mut exported = snapshot_json(&display);
                exported["view"] = json!({
                    "search":search,"sort":sort,"total_groups":snapshot.groups.len(),
                    "matched_groups":matched,"shown_groups":shown
                });
                output::write_json(&exported)
            } else {
                output::write_text(&format!(
                    "View: {shown}/{matched} matching groups · {} total · Sort: {} · Search: {}\n{}{}",
                    snapshot.groups.len(),
                    match sort { GroupSort::Memory => "memory", GroupSort::Name => "name" },
                    if search.is_empty() { "(all)".into() } else { format!("{search:?}") },
                    list_text(&display, limit),
                    if matched == 0 { "No matching groups in this sample. Try another name, bundle ID or exact PID.\n" } else { "" }
                ))
            }
        }
        Command::Inspect { id, json } => {
            let exported = output::export_snapshot(&snapshot, true);
            let mut text = output::inspect_text(&snapshot, &id)?;
            if json {
                let group = exported.groups.iter().find(|g| g.id == id);
                let processes: Vec<_> = exported
                    .processes
                    .iter()
                    .filter(|p| p.id == id || group.is_some_and(|g| g.process_ids.contains(&p.id)))
                    .collect();
                let mut report = json!({"schema_version":SCHEMA_VERSION,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"group":group,"processes":processes});
                add_labels(&mut report, &snapshot);
                output::write_json(&report)
            } else {
                for process in snapshot.processes.iter().filter(|process| {
                    process.id == id
                        || snapshot
                            .groups
                            .iter()
                            .any(|group| group.id == id && group.process_ids.contains(&process.id))
                }) {
                    if let Some(label) = label_process(process) {
                        text.push_str(&format!(
                            "Developer tool: {}\nEvidence: {}\n",
                            label.label, label.evidence
                        ));
                    }
                }
                output::write_text(&text)
            }
        }
        Command::Doctor { json } => {
            let report = json!({
                "schema_version": SCHEMA_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
                "architecture": std::env::consts::ARCH,
                "sampled_at_unix_ms": snapshot.sampled_at_unix_ms,
                "coverage": snapshot.coverage,
                "diagnostics": snapshot.diagnostics,
                "capabilities": {
                    "read_only": true,
                    "ai_attribution_enabled": true,
                    "background_service": false
                },
                "notes": [
                    "Bree inspects and exports memory usage without saving state or quitting applications.",
                    "Developer labels cover only native Claude installations under ~/.local/share/claude/versions/<numeric version> and Codex App CLI paths. They do not establish project ownership, task completion, sharing or permission to stop a task.",
                    "Each supported environment requires testing. A local run does not verify other system versions.",
                    "Bree does not request root, Full Disk Access, Accessibility or Automation permissions."
                ]
            });
            if json {
                output::write_json(&report)
            } else {
                output::write_text(&format!(
                    "{}\n\nCapabilities: read-only memory inspection and limited developer-tool labels. Bree saves no state and does not quit applications. Background services are disabled.\n{}",
                    output::status_text(&snapshot),
                    snapshot
                        .coverage
                        .notes
                        .iter()
                        .chain(snapshot.diagnostics.iter())
                        .map(|n| format!("· {}", bree_cli::model::safe_text(n)))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
        }
        Command::License { .. } => unreachable!(),
        Command::Watch { .. } => unreachable!(),
    }.map(|()|0)
}

fn snapshot_json(snapshot: &Snapshot) -> serde_json::Value {
    let mut value = serde_json::to_value(output::export_snapshot(snapshot, false))
        .expect("serializable snapshot");
    add_labels(&mut value, snapshot);
    value
}

fn add_labels(value: &mut serde_json::Value, snapshot: &Snapshot) {
    if let Some(processes) = value["processes"].as_array_mut() {
        for process in processes {
            let label = snapshot
                .processes
                .iter()
                .find(|source| Some(source.id.as_str()) == process["id"].as_str())
                .and_then(label_process);
            process["development_label"] = json!(label);
        }
    }
}

fn list_text(snapshot: &Snapshot, limit: Option<usize>) -> String {
    let mut text = format!(
        "{}\n\nMemory\tInstances\tCategory\tName\tObject ID\n",
        output::status_text(snapshot)
    );
    for group in snapshot
        .groups
        .iter()
        .take(limit.unwrap_or(snapshot.groups.len()))
    {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            output::metric_bytes(&group.memory_bytes),
            group.process_ids.len(),
            output::category_text(group.category),
            safe_text(&group.name),
            safe_text(&group.id)
        ));
    }
    text.push_str("Use inspect for individual details. Developer labels do not establish task completion or permission to stop a task.\n");
    text
}
