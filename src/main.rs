use bree_cli::{
    attribution::label_process,
    cleanup::{self, CleanupMode, Session},
    collect::Collector,
    history,
    model::{SCHEMA_VERSION, Snapshot, safe_text},
    output,
    policy::{CleanupPlan, Disposition, PolicyContext, PolicyState, evaluate},
    preview,
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
    about = "看清本机内存占用 · 处理闭环 Alpha",
    long_about = "本地内存查看、规则和处理记录。交互终端输入 bree 进入菜单；A1 能力仍关闭，不发送应用退出请求。"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 单次系统内存概况
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 按有证据的应用分组查看全部占用；未知归属独立保留
    List {
        #[arg(long)]
        json: bool,
        /// 限制显示的组数；JSON 中仍保留完整进程表及覆盖统计
        #[arg(long, value_parser = clap::value_parser!(usize))]
        limit: Option<usize>,
    },
    /// 查看 list 返回的对象 ID；实时重采样并匹配当前实例
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// 前台持续观察；管道模式输出文本或 JSONL
    Watch {
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
        interval: u64,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        count: Option<u64>,
    },
    /// 检查采集覆盖与当前能力；不会申请权限或执行清理
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// 冻结处理计划；退出能力未通过的对象仍跳过
    Clean {
        #[arg(long)]
        dry_run: bool,
        /// 明确发起非交互批次；不能绕过策略或能力
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 查看完成／未完成的处理记录；绝不重放请求
    History {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(usize))]
        limit: usize,
    },
    /// 显示 GPL-3.0 许可；第三方声明随程序附带，可离线查看
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
            "bree · 处理闭环 Alpha\n用法：bree status | list | inspect <id> | watch | doctor | clean --dry-run | history\n非交互批次使用 clean --yes --json；能力闸门不会被 --yes 绕过。交互终端输入 bree 进入菜单。",
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
        let message = "非交互 clean 需要 --yes；仅预演请使用 --dry-run";
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
        eprintln!("bree: 无法注册取消处理：{error}");
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
                format!("命令取消；{error}")
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
        Command::List { json: true, limit } => {
            let mut exported = snapshot_json(&snapshot, &observation);
            if let Some(limit) = limit {
                exported["groups"]
                    .as_array_mut()
                    .expect("snapshot groups")
                    .truncate(limit);
            }
            output::write_json(&exported)
        }
        Command::List { json: false, limit } => {
            output::write_text(&classified_list_text(&snapshot, limit, &observation))
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
                    "\n策略：{}\n{}\n",
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
                            "开发工具：{}\n依据：{}\n",
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
            let report = json!({"schema_version":SCHEMA_VERSION,"version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"sampled_at_unix_ms":snapshot.sampled_at_unix_ms,"coverage":snapshot.coverage,"diagnostics":snapshot.diagnostics,"capabilities":{"read_only":!cleanup::enabled(),"rules_enabled":true,"dry_run_enabled":true,"cleanup_session_enabled":true,"history_enabled":true,"cleanup_enabled":cleanup::enabled(),"a1_enabled":cleanup::enabled(),"a2_enabled":false,"ai_attribution_enabled":true,"background_service":false},"capability_gate_reason":cleanup::capability_reason(),"rule_storage":storage,"policy_state_valid":observation.error.is_none(),"policy_error":observation.error,"notes":["规则、预演、批次结果与历史可用；正常退出能力仍关闭，A2 未启用。","开发标签仅限已验证的 Claude 2.1.292 原生安装与 Codex App CLI 路径；不是项目、完成、共享或停止证据。","doctor 不通过权限位承诺写入成功，实际写入仍可能失败。","拟支持系统须逐环境实测；本机运行不代表跨版本验收。","不申请 root、完全磁盘访问、辅助功能或自动化权限。"]});
            if json {
                output::write_json(&report)
            } else {
                output::write_text(&format!(
                    "{}\n{}\n\n能力：规则、预演、批次结果与历史、有限开发工具标签；实际清理 A1/A2、后台服务均未启用。\n存储：{}\n{}",
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
                    "Bree 清理预演 · 只读，不发送请求\n规则版本 {} · revision {} · {}\n自动 {} · 待确认规则 {} · 保护 {}\n{}",
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
                                .join("；")
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
        Disposition::Automatic => "自动",
        Disposition::Pending => "待确认规则",
        Disposition::Protected => "保护／只读",
    }
}

fn policy_summary(observation: &RuleObservation) -> String {
    let mut text = format!(
        "规则 revision {} · 自动 {} · 待确认规则 {} · 保护 {} · 正常退出未启用",
        observation.plan.rule_revision,
        observation.plan.automatic_count,
        observation.plan.pending_count,
        observation.plan.protected_count
    );
    if let Some(error) = &observation.error {
        text.push_str(&format!(
            "\n规则无效：{error}；查看继续，设置与预演保持关闭。"
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
        "{}\n{}\n\n占用\t实例\t分类\t策略\t名称\t对象 ID\n",
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
            .unwrap_or("未知");
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
    text.push_str("逐项判定原因见 inspect；开发标签不代表任务完成或可停止。\n");
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
            "Bree 处理结果 · {}\n{}\n{}\n{}",
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
                .map(|e| format!("记录／观察缺口：{}", safe_text(e)))
                .collect::<Vec<_>>()
                .join("\n")
        ))?;
    }
    Ok(result.exit_code())
}
fn history_text(items: &[history::HistoryItem]) -> String {
    let mut text = "Bree 处理记录 · 只读，不重放动作\n".to_string();
    if items.is_empty() {
        text.push_str("暂无处理记录。\n");
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
                    "  {}：{}；{}\n",
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
