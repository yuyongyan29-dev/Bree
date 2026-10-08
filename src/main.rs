use bree_cli::{
    attribution::label_process,
    cleanup::{self, CleanupMode, Session},
    collect::Collector,
    history,
    model::{SCHEMA_VERSION, Snapshot, safe_text},
    output,
    policy::{CleanupPlan, Disposition, PolicyContext, PolicyState, evaluate},
    preview,
    query::{GroupSort, compare_groups, group_matches, validate_search},
    storage::Store,
    tui,
};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    io::{self, IsTerminal},
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
    long_about = "Inspect local memory usage, application rules and cleanup records. Run bree in an interactive terminal to open the menu. Application quit requests are disabled in this Alpha."
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
    /// Freeze a cleanup plan; skip targets without verified quit capability
    Clean {
        #[arg(long)]
        dry_run: bool,
        /// Start a noninteractive session; policy and capability checks still apply
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show completed and incomplete cleanup records without replaying requests
    History {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(usize))]
        limit: usize,
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
            "bree · Alpha\nUsage: bree status | list | inspect <id> | watch | doctor | clean --dry-run | history | license\nFor a noninteractive session, use clean --yes --json. Capability checks still apply. Run bree in an interactive terminal to open the menu.",
        ) {
            Ok(()) => 0,
            Err(_) => 1,
        };
        std::process::exit(code);
    }
    let clean_ui = tty
        && matches!(
            cli.command,
            Some(Command::Clean {
                dry_run: false,
                yes: false,
                json: false
            })
        );
    if matches!(
        cli.command,
        Some(Command::Clean {
            dry_run: false,
            yes: false,
            ..
        })
    ) && !clean_ui
    {
        let message = "Noninteractive clean requires --yes; use --dry-run to preview";
        if matches!(cli.command, Some(Command::Clean { json: true, .. })) {
            let _ = output::write_json(
                &json!({"schema_version":SCHEMA_VERSION,"error":{"code":"invalid_arguments","message":message}}),
            );
        }
        eprintln!("bree: {message}");
        std::process::exit(2);
    }
    let interactive = cli.command.is_none()
        || clean_ui
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
        | Some(Command::History { json, .. })
        | Some(Command::License { json, .. })
        | Some(Command::Clean { json, .. }) => *json,
        None => false,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let action_active = Arc::new(AtomicBool::new(matches!(
        cli.command,
        Some(Command::Clean { dry_run: false, .. })
    )));
    let cancel_signal = Arc::clone(&cancelled);
    let action_signal = Arc::clone(&action_active);
    if !interactive
        && let Err(error) = ctrlc::set_handler(move || {
            if action_signal.load(Ordering::Acquire) {
                cancel_signal.store(true, Ordering::Release);
            } else {
                // A blocked output consumer must not prevent cancellation.
                std::process::exit(130);
            }
        })
    {
        eprintln!("bree: Cannot register the cancellation handler: {error}");
        std::process::exit(1);
    }
    let result = execute(cli.command, tty, &cancelled, &action_active);
    action_active.store(false, Ordering::Release);
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
            eprintln!("bree: {}", bree_cli::model::safe_text(&message));
            if was_cancelled { 130 } else { 1 }
        }
    };
    std::process::exit(code);
}

fn execute(
    command: Option<Command>,
    tty: bool,
    cancelled: &Arc<AtomicBool>,
    action_active: &AtomicBool,
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
    if let Some(Command::Clean {
        dry_run: false,
        yes: false,
        json: false,
    }) = &command
        && tty
    {
        cancelled.store(tui::run_clean()?, Ordering::Relaxed);
        return Ok(0);
    }
    if let Some(Command::History { json, limit }) = &command {
        let items = history::load(&Store::from_env()?, *limit)?;
        if *json {
            output::write_json(
                &json!({"schema_version":SCHEMA_VERSION,"records":items,"replay_enabled":false}),
            )?;
        } else {
            output::write_text(&history_text(&items))?;
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
                output::write_json(&snapshot_json(&snapshot, &observe_rules(&snapshot)))?;
            } else {
                let observation = observe_rules(&snapshot);
                output::write_text(&format!(
                    "{}\n{}",
                    output::status_text(&snapshot),
                    policy_summary(&observation)
                ))?;
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
    let observation = observe_rules(&snapshot);
    match command.expect("interactive handled above") {
        Command::Status { json: true } => output::write_json(
            &json!({"schema_version":SCHEMA_VERSION,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"collected_in_ms":snapshot.collected_in_ms,"system":snapshot.system,"coverage":snapshot.coverage,"diagnostics":snapshot.diagnostics,"policy_summary":{"automatic":observation.plan.automatic_count,"pending":observation.plan.pending_count,"protected":observation.plan.protected_count,"revision":observation.plan.rule_revision},"policy_state_valid":observation.error.is_none(),"policy_error":observation.error}),
        ),
        Command::Status { json: false } => output::write_text(&format!(
            "{}\n{}",
            output::status_text(&snapshot),
            policy_summary(&observation)
        )),
        Command::List {
            json,
            limit,
            search,
            sort,
        } => {
            // This display view keeps the complete process sample and evaluated plan.
            // Filtering cannot change ownership, policy counts or cleanup candidates.
            let mut display = snapshot.clone();
            display.groups.retain(|group| group_matches(&snapshot, group, &search));
            display.groups.sort_by(|a, b| compare_groups(a, b, sort));
            let matched = display.groups.len();
            let shown = limit.unwrap_or(matched).min(matched);
            if json {
                display.groups.truncate(shown);
                let mut exported = snapshot_json(&display, &observation);
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
                    classified_list_text(&display, limit, &observation),
                    if matched == 0 { "No matching groups in this sample. Try another name, bundle ID or exact PID.\n" } else { "" }
                ))
            }
        }
        Command::Inspect { id, json } => {
            let exported = output::export_snapshot(&snapshot, true);
            let mut text = output::inspect_text(&snapshot, &id)?;
            let entry = observation.plan.entries.iter().find(|entry| {
                entry.group_id == id
                    || snapshot
                        .groups
                        .iter()
                        .any(|group| group.id == entry.group_id && group.process_ids.contains(&id))
            });
            if let Some(entry) = entry {
                text.push_str(&format!(
                    "\nPolicy: {}\n{}\n",
                    disposition_text(entry.disposition),
                    entry
                        .reasons
                        .iter()
                        .map(|reason| format!("· {}", safe_text(reason)))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            if json {
                let group = exported.groups.iter().find(|g| g.id == id);
                let processes: Vec<_> = exported
                    .processes
                    .iter()
                    .filter(|p| p.id == id || group.is_some_and(|g| g.process_ids.contains(&p.id)))
                    .collect();
                let mut report = json!({"schema_version":SCHEMA_VERSION,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"group":group,"processes":processes,"policy":entry,"policy_state_valid":observation.error.is_none(),"policy_error":observation.error});
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
            let storage = match Store::from_env() {
                Ok(store) => {
                    scrub_paths(serde_json::to_value(store.probe()).map_err(|e| e.to_string())?)
                }
                Err(error) => json!({"error":safe_text(&error),"write_status":"not_probed"}),
            };
            let report = json!({"schema_version":SCHEMA_VERSION,"version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"coverage":snapshot.coverage,"diagnostics":snapshot.diagnostics,"capabilities":{"read_only":!cleanup::enabled(),"rules_enabled":true,"dry_run_enabled":true,"cleanup_session_enabled":true,"history_enabled":true,"cleanup_enabled":cleanup::enabled(),"a1_enabled":cleanup::enabled(),"a2_enabled":false,"ai_attribution_enabled":true,"background_service":false},"capability_gate_reason":cleanup::capability_reason(),"rule_storage":storage,"policy_state_valid":observation.error.is_none(),"policy_error":observation.error,"notes":["Rules, dry-run, session results and history are available. Application quitting and AI task reclamation remain disabled.","Developer labels cover only native Claude installations under ~/.local/share/claude/versions/<numeric version> and Codex App CLI paths. They do not establish project ownership, task completion, sharing or permission to stop a task.","Permission bits do not guarantee a successful write; storage writes may still fail.","Each supported environment requires testing. A local run does not verify other system versions.","Bree does not request root, Full Disk Access, Accessibility or Automation permissions."]});
            if json {
                output::write_json(&report)
            } else {
                output::write_text(&format!(
                    "{}\n{}\n\nCapabilities: rules, dry-run, session results, history and limited developer-tool labels. Application quitting, AI task reclamation and background services are disabled.\nStorage: {}\n{}",
                    output::status_text(&snapshot),
                    policy_summary(&observation),
                    report["rule_storage"],
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
        Command::Clean {
            dry_run: true,
            json,
            ..
        } => {
            let store = Store::from_env()?;
            let plan = preview::prepare(&store, &snapshot)?;
            if json {
                output::write_json(&plan)
            } else {
                output::write_text(&format!(
                    "Bree cleanup preview · Read-only; no requests sent\nPolicy version {} · revision {} · {}\nAutomatic {} · Needs review {} · Protected {}\n{}",
                    plan.policy_version,
                    plan.rule_revision,
                    plan.plan_id,
                    plan.automatic_count,
                    plan.pending_count,
                    plan.protected_count,
                    plan.entries
                        .iter()
                        .map(|entry| format!(
                            "{}\t{}\t{}",
                            disposition_text(entry.disposition),
                            safe_text(&entry.name),
                            entry
                                .reasons
                                .iter()
                                .map(|reason| safe_text(reason))
                                .collect::<Vec<_>>()
                                .join("; ")
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
        }
        Command::Clean { dry_run: false, json, .. } => {
            return run_batch(Store::from_env()?, snapshot, json, cancelled, action_active);
        },
        Command::History { .. } => unreachable!(),
        Command::License { .. } => unreachable!(),
        Command::Watch { .. } => unreachable!(),
    }.map(|()|0)
}

struct RuleObservation {
    plan: CleanupPlan,
    error: Option<String>,
}

fn observe_rules(snapshot: &Snapshot) -> RuleObservation {
    match Store::from_env().and_then(|store| store.load()) {
        Ok(state) => RuleObservation {
            plan: evaluate(
                snapshot,
                &state,
                &PolicyContext {
                    state_valid: true,
                    a1_enabled: false,
                },
            ),
            error: None,
        },
        Err(error) => RuleObservation {
            plan: evaluate(snapshot, &PolicyState::default(), &PolicyContext::default()),
            error: Some(safe_text(&error)),
        },
    }
}

fn disposition_text(value: Disposition) -> &'static str {
    match value {
        Disposition::Automatic => "Automatic",
        Disposition::Pending => "Needs review",
        Disposition::Protected => "Protected / read-only",
    }
}

fn policy_summary(observation: &RuleObservation) -> String {
    let mut text = format!(
        "Rule revision {} · Automatic {} · Needs review {} · Protected {} · Application quitting disabled",
        observation.plan.rule_revision,
        observation.plan.automatic_count,
        observation.plan.pending_count,
        observation.plan.protected_count
    );
    if let Some(error) = &observation.error {
        text.push_str(&format!(
            "\nInvalid rules: {error}; inspection remains available, but settings and preview are disabled."
        ));
    }
    text
}

fn snapshot_json(snapshot: &Snapshot, observation: &RuleObservation) -> serde_json::Value {
    let mut value = serde_json::to_value(output::export_snapshot(snapshot, false))
        .expect("serializable snapshot");
    value["policy"] = serde_json::to_value(&observation.plan).expect("serializable plan");
    value["policy_state_valid"] = json!(observation.error.is_none());
    value["policy_error"] = json!(observation.error);
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

fn classified_list_text(
    snapshot: &Snapshot,
    limit: Option<usize>,
    observation: &RuleObservation,
) -> String {
    let mut text = format!(
        "{}\n{}\n\nMemory\tInstances\tCategory\tPolicy\tName\tObject ID\n",
        output::status_text(snapshot),
        policy_summary(observation)
    );
    for group in snapshot
        .groups
        .iter()
        .take(limit.unwrap_or(snapshot.groups.len()))
    {
        let state = observation
            .plan
            .entries
            .iter()
            .find(|entry| entry.group_id == group.id)
            .map(|entry| disposition_text(entry.disposition))
            .unwrap_or("Unknown");
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            output::metric_bytes(&group.memory_bytes),
            group.process_ids.len(),
            output::category_text(group.category),
            state,
            safe_text(&group.name),
            safe_text(&group.id)
        ));
    }
    text.push_str("Use inspect for individual reasons. Developer labels do not establish task completion or permission to stop a task.\n");
    text
}

fn scrub_paths(mut value: serde_json::Value) -> serde_json::Value {
    fn scrub(value: &mut serde_json::Value, home: Option<&str>) {
        match value {
            serde_json::Value::String(text) => *text = output::redact_path(text, home),
            serde_json::Value::Array(values) => values.iter_mut().for_each(|v| scrub(v, home)),
            serde_json::Value::Object(values) => values.values_mut().for_each(|v| scrub(v, home)),
            _ => {}
        }
    }
    let home = std::env::var("HOME").ok();
    scrub(&mut value, home.as_deref());
    value
}

fn run_batch(
    store: Store,
    snapshot: Snapshot,
    json: bool,
    cancelled: &Arc<AtomicBool>,
    action_active: &AtomicBool,
) -> Result<i32, String> {
    let mut session = Session::start(store, snapshot, CleanupMode::Automatic)?;
    session.set_cancel_flag(Arc::clone(cancelled));
    while session.result().is_none() {
        if cancelled.load(Ordering::Acquire) {
            session.cancel();
        } else {
            session.tick();
        }
        if session.result().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let result = session.result().expect("finished session");
    action_active.store(false, Ordering::Release);
    if json {
        output::write_json(result)?;
    } else {
        output::write_text(&format!(
            "Bree cleanup results · {}\n{}\n{}\n{}",
            result.run_id,
            result
                .targets
                .iter()
                .map(|t| format!(
                    "{}\t{}\t{}",
                    safe_text(&t.name),
                    t.outcome.text(),
                    safe_text(&t.reason)
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            safe_text(&result.resource_observation),
            result
                .errors
                .iter()
                .map(|e| format!("Recording or observation gap: {}", safe_text(e)))
                .collect::<Vec<_>>()
                .join("\n")
        ))?;
    }
    Ok(result.exit_code())
}
fn history_text(items: &[history::HistoryItem]) -> String {
    let mut text = "Bree cleanup history · Read-only; actions are never replayed\n".to_string();
    if items.is_empty() {
        text.push_str("No cleanup records yet.\n");
    }
    for item in items {
        text.push_str(&format!(
            "{}\t{}\t{}\n{}\n",
            safe_text(&item.run_id),
            item.timestamp_unix_ms,
            safe_text(&item.status),
            safe_text(&item.message)
        ));
        if let Some(result) = &item.result {
            for target in &result.targets {
                text.push_str(&format!(
                    "  {}: {}; {}\n",
                    safe_text(&target.name),
                    target.outcome.text(),
                    safe_text(&target.reason)
                ));
            }
            text.push_str(&format!("  {}\n", safe_text(&result.resource_observation)));
        }
    }
    text
}
