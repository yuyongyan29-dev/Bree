//! Inspection, exact installation rules, preview and time-driven cleanup UI.
//! A snapshot worker never owns or changes terminal state.

use std::io::{self, IsTerminal, Stdout};
use std::panic;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs, Wrap,
    },
};

use crate::attribution::label_process;
use crate::cleanup::{self, CleanupMode, CleanupResult, Session};
use crate::collect::{Collector, pump_platform_events};
use crate::history::{self, HistoryItem};
use crate::model::{Category, OccupancyGroup, ProcessIdentity, ProcessInfo, Snapshot, safe_text};
use crate::output::{metric_bytes, pressure_text};
use crate::policy::{
    AppScope, CleanupPlan, Disposition, PlanEntry, PolicyContext, PolicyState, RuleAction,
    RuleChange, evaluate, scope_for_group,
};
use crate::storage::Store;

const ACCENT: Color = Color::Rgb(190, 86, 24);
const FOREGROUND: Color = Color::Reset;
const MUTED: Color = Color::Reset;
const BACKGROUND: Color = Color::Reset;
const SELECTED: Color = Color::Reset;
const MIN_WIDTH: u16 = 48;
const MIN_HEIGHT: u16 = 16;

type PanicHook = dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static;

struct TerminalGuard {
    previous_hook: Arc<PanicHook>,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
}

fn exit_for_external_signal() -> ! {
    // ctrlc invokes this on its own ordinary thread, not in an async signal handler.
    // Crossterm 0.29's Unix reader can loop forever on EOF. Do not depend on a flag
    // being observed by that reader, or on a stdout lock held by a blocked draw.
    let _ = disable_raw_mode();
    #[cfg(unix)]
    unsafe {
        let flags = libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL);
        if flags >= 0
            && libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
        {
            let sequence = b"\x1b[?25h\x1b[?1049l";
            let _ = libc::write(
                libc::STDOUT_FILENO,
                sequence.as_ptr().cast(),
                sequence.len(),
            );
            // A terminal descriptor may share an open-file description with the shell.
            // Restore flags even when the best-effort nonblocking write fails.
            let _ = libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags);
        }
    }
    // No subprocesses exist; terminating the process also ends a read-only worker.
    std::process::exit(130)
}

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("无法进入终端输入模式：{error}"))?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(format!("无法初始化终端：{error}"));
        }
        let previous_hook: Arc<PanicHook> = Arc::from(panic::take_hook());
        let panic_hook = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            panic_hook(info);
        }));
        Ok(Self { previous_hook })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        if !thread::panicking() {
            let previous_hook = Arc::clone(&self.previous_hook);
            panic::set_hook(Box::new(move |info| previous_hook(info)));
        }
    }
}

enum Request {
    Refresh,
    Stop,
}

struct Worker {
    request: mpsc::Sender<Request>,
    response: mpsc::Receiver<Result<Snapshot, String>>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn new(mut collector: Collector) -> Self {
        let (request, requests) = mpsc::channel();
        let (responses, response) = mpsc::channel();
        let handle = thread::spawn(move || {
            while let Ok(request) = requests.recv() {
                match request {
                    Request::Stop => break,
                    Request::Refresh => {
                        if responses.send(collector.snapshot()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            request,
            response,
            handle: Some(handle),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.request.send(Request::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Page {
    Home,
    Resources,
    Detail(String),
    Settings,
    RuleDetail(String),
    Preview,
    Executing,
    Results,
    History,
    HistoryDetail(String),
}

enum RuleConfirmation {
    Add {
        action: RuleAction,
        scope: AppScope,
        name: String,
    },
    Remove {
        id: String,
        action: RuleAction,
        scope: AppScope,
    },
    Exit {
        group_id: String,
        name: String,
        scope: AppScope,
        identity: ProcessIdentity,
        snapshot: Box<Snapshot>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Application,
    Ai,
    System,
    Pending,
}

impl Filter {
    const ALL: [Self; 5] = [
        Self::All,
        Self::Application,
        Self::Ai,
        Self::System,
        Self::Pending,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|value| *value == self)
            .unwrap_or(0)
    }

    fn accepts(self, category: Category) -> bool {
        match self {
            Self::All => true,
            Self::Application => category == Category::Application,
            Self::Ai => category == Category::AiDevelopment,
            Self::System => category == Category::System,
            Self::Pending => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    Memory,
    Name,
}

enum InputResult {
    Continue,
    Refresh,
    Quit(bool),
}

struct App {
    page: Page,
    snapshot: Option<Snapshot>,
    loading: bool,
    error: Option<String>,
    menu: ListState,
    table: TableState,
    filter: Filter,
    sort: Sort,
    selected_group: Option<String>,
    detail_scroll: u16,
    notice: Option<String>,
    sampled: Option<Instant>,
    refresh_interval: Duration,
    store: Option<Store>,
    policy: PolicyState,
    policy_error: Option<String>,
    plan: Option<CleanupPlan>,
    preview_plan: Option<CleanupPlan>,
    preview_error: Option<String>,
    preview_sample: Option<String>,
    preview_refresh_queued: bool,
    rules: ListState,
    confirmation: Option<RuleConfirmation>,
    confirmation_scroll: u16,
    cleanup: Option<Session>,
    cleanup_result: Option<CleanupResult>,
    history: Vec<HistoryItem>,
    history_error: Option<String>,
    history_selection: ListState,
    start_cleanup_after_sample: bool,
}

impl App {
    fn new(watch: bool) -> Self {
        let mut menu = ListState::default();
        menu.select(Some(2));
        Self {
            page: if watch { Page::Resources } else { Page::Home },
            snapshot: None,
            loading: true,
            error: None,
            menu,
            table: TableState::default(),
            filter: Filter::All,
            sort: Sort::Memory,
            selected_group: None,
            detail_scroll: 0,
            notice: None,
            sampled: None,
            refresh_interval: Duration::from_secs(2),
            store: None,
            policy: PolicyState::default(),
            policy_error: None,
            plan: None,
            preview_plan: None,
            preview_error: None,
            preview_sample: None,
            preview_refresh_queued: false,
            rules: ListState::default(),
            confirmation: None,
            confirmation_scroll: 0,
            cleanup: None,
            cleanup_result: None,
            history: Vec::new(),
            history_error: None,
            history_selection: ListState::default(),
            start_cleanup_after_sample: false,
        }
    }

    fn initialize_store(&mut self) {
        match Store::from_env() {
            Ok(store) => {
                self.store = Some(store);
                self.reload_policy();
            }
            Err(error) => {
                self.policy_error = Some(safe_text(&error));
                self.evaluate_policy();
            }
        }
    }

    fn reload_policy(&mut self) {
        let selected = self
            .rules
            .selected()
            .and_then(|index| self.policy.rules.get(index))
            .map(|rule| rule.id.clone());
        if let Some(store) = &self.store {
            match store.load() {
                Ok(state) => {
                    self.policy = state;
                    self.policy_error = None;
                }
                Err(error) => self.policy_error = Some(safe_text(&error)),
            }
        }
        let index = selected
            .as_ref()
            .and_then(|id| self.policy.rules.iter().position(|rule| &rule.id == id))
            .or_else(|| (!self.policy.rules.is_empty()).then_some(0));
        self.rules.select(index);
        self.evaluate_policy();
    }

    fn evaluate_policy(&mut self) {
        self.plan = self.snapshot.as_ref().map(|snapshot| {
            evaluate(
                snapshot,
                &self.policy,
                &PolicyContext {
                    state_valid: self.policy_error.is_none(),
                    a1_enabled: cleanup::enabled(),
                },
            )
        });
    }

    fn entry(&self, id: &str) -> Option<&PlanEntry> {
        self.plan
            .as_ref()
            .and_then(|plan| plan.entries.iter().find(|entry| entry.group_id == id))
    }

    fn group_has_development_label(&self, group: &OccupancyGroup) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.processes.iter().any(|process| {
                group.process_ids.contains(&process.id) && label_process(process).is_some()
            })
        })
    }

    fn begin_add(&mut self, id: &str, action: RuleAction) {
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some(
                "规则存储不可用，当前保持只读。请在设置页查看原因；不会覆盖损坏的配置。".into(),
            );
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        match scope_for_group(snapshot, id) {
            Ok(scope) => {
                let name = snapshot
                    .groups
                    .iter()
                    .find(|group| group.id == id)
                    .map(|group| safe_text(&group.name))
                    .unwrap_or_default();
                self.confirmation = Some(RuleConfirmation::Add {
                    action,
                    scope,
                    name,
                });
                self.confirmation_scroll = 0;
            }
            Err(error) => {
                self.notice = Some(format!("无法为此对象设置应用规则：{}", safe_text(&error)))
            }
        }
    }

    fn begin_remove(&mut self) {
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some("规则存储不可用，无法修改。修复原配置后按 R 重新读取。".into());
            return;
        }
        let rule = match &self.page {
            Page::RuleDetail(id) => self.policy.rules.iter().find(|rule| &rule.id == id),
            _ => self
                .rules
                .selected()
                .and_then(|index| self.policy.rules.get(index)),
        };
        if let Some(rule) = rule {
            self.confirmation = Some(RuleConfirmation::Remove {
                id: rule.id.clone(),
                action: rule.action,
                scope: rule.scope.clone(),
            });
            self.confirmation_scroll = 0;
        }
    }

    fn begin_exit(&mut self, id: &str) {
        if !cleanup::enabled() {
            self.notice =
                Some("正常退出能力尚未启用，当前保持只读。允许规则不会开启这项能力。".into());
            return;
        }
        self.reload_policy();
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some("规则存储不可读取，正常退出已禁用。请在设置页查看原因。".into());
            return;
        }
        if let Some(entry) = self.entry(id)
            && entry.disposition == Disposition::Protected
        {
            self.notice = Some(format!(
                "此对象受保护，不能请求退出：{}",
                entry
                    .reasons
                    .iter()
                    .map(|reason| safe_text(reason))
                    .collect::<Vec<_>>()
                    .join("；")
            ));
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let scope = match scope_for_group(snapshot, id) {
            Ok(scope) => scope,
            Err(error) => {
                self.notice = Some(format!("此对象不支持正常退出：{}", safe_text(&error)));
                return;
            }
        };
        let Some(entry) = self.entry(id) else {
            return;
        };
        let Some(identity) = &entry.target_identity else {
            self.notice = Some("缺少确切主应用实例，无法请求退出。".into());
            return;
        };
        self.confirmation = Some(RuleConfirmation::Exit {
            group_id: id.into(),
            name: entry.name.clone(),
            scope,
            identity: identity.clone(),
            snapshot: Box::new(snapshot.clone()),
        });
        self.confirmation_scroll = 0;
    }

    fn start_automatic_cleanup(&mut self) {
        self.start_cleanup_after_sample = false;
        self.reload_policy();
        if !cleanup::enabled() {
            self.notice = Some(
                "正常退出能力尚未启用，暂无可自动处理的对象；没有发送任何请求。P 可查看只读预演。"
                    .into(),
            );
            return;
        }
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some("规则存储不可读取，当前禁止清理；S 可查看原因。".into());
            return;
        }
        if self
            .plan
            .as_ref()
            .is_none_or(|plan| plan.automatic_count == 0)
        {
            self.notice = Some("暂无可自动处理的对象，没有发送请求。可在详情主动设置允许规则，或按 P 查看分类理由。".into());
            return;
        }
        if let Some(snapshot) = self.snapshot.clone() {
            self.start_cleanup(snapshot, CleanupMode::Automatic);
        }
    }

    fn start_cleanup(&mut self, snapshot: Snapshot, mode: CleanupMode) {
        let Some(store) = self.store.clone() else {
            self.notice = Some("清理未开始：记录存储不可用；没有发送退出请求。".into());
            return;
        };
        match Session::start(store, snapshot, mode) {
            Ok(session) => {
                self.cleanup = Some(session);
                self.cleanup_result = None;
                self.preview_refresh_queued = false;
                self.page = Page::Executing;
                self.detail_scroll = 0;
            }
            Err(error) => {
                self.notice = Some(format!(
                    "清理未开始：{}；没有扩大到其他实例。",
                    safe_text(&error)
                ))
            }
        }
    }

    fn finish_cleanup(&mut self) -> bool {
        let result = self
            .cleanup
            .as_ref()
            .and_then(|session| session.result())
            .cloned();
        if let Some(result) = result {
            self.cleanup_result = Some(result);
            self.cleanup = None;
            self.page = Page::Results;
            self.detail_scroll = 0;
            true
        } else {
            false
        }
    }

    fn advance_cleanup(&mut self) -> bool {
        let Some(session) = self.cleanup.as_mut() else {
            return false;
        };
        let before = session.progress().to_owned();
        session.tick();
        let changed = session.progress() != before;
        self.finish_cleanup() || changed
    }

    fn cancel_cleanup(&mut self) {
        if let Some(session) = &mut self.cleanup {
            session.cancel();
        }
        self.finish_cleanup();
    }

    fn open_history(&mut self) {
        self.start_cleanup_after_sample = false;
        self.page = Page::History;
        self.detail_scroll = 0;
        self.reload_history();
    }

    fn reload_history(&mut self) {
        let selected = self
            .history_selection
            .selected()
            .and_then(|index| self.history.get(index))
            .map(|item| item.run_id.clone());
        let Some(store) = &self.store else {
            self.history_error = Some("记录存储不可用".into());
            return;
        };
        match history::load(store, 50) {
            Ok(items) => {
                self.history = items;
                self.history_error = None;
            }
            Err(error) => self.history_error = Some(safe_text(&error)),
        }
        let index = selected
            .as_ref()
            .and_then(|id| self.history.iter().position(|item| &item.run_id == id))
            .or_else(|| (!self.history.is_empty()).then_some(0));
        self.history_selection.select(index);
    }

    fn confirm_change(&mut self) {
        if matches!(self.confirmation, Some(RuleConfirmation::Exit { .. })) {
            if let Some(RuleConfirmation::Exit {
                group_id, snapshot, ..
            }) = self.confirmation.take()
            {
                self.start_cleanup(*snapshot, CleanupMode::Manual { group_id });
            }
            return;
        }
        self.reload_policy();
        if self.policy_error.is_some() {
            self.confirmation = None;
            self.notice = Some("规则状态读取失败，保存已取消；保持只读。请查看设置页原因。".into());
            return;
        }
        let Some(confirmation) = self.confirmation.take() else {
            return;
        };
        let change = match confirmation {
            RuleConfirmation::Add { action, scope, .. } => RuleChange::Add { action, scope },
            RuleConfirmation::Remove { id, .. } => RuleChange::Remove { id },
            RuleConfirmation::Exit { .. } => unreachable!(),
        };
        let Some(store) = &self.store else {
            return;
        };
        match store.change(change) {
            Ok(state) => {
                self.policy = state;
                self.reload_policy();
                self.reconcile_selection();
                self.notice = Some("规则已保存。这里只改变后续分类，不会向应用发送退出请求。可在设置页移除该规则。".into());
            }
            Err(error) => {
                self.policy_error = Some(safe_text(&error));
                self.evaluate_policy();
                self.notice = Some(format!("规则保存失败，保持只读：{}", safe_text(&error)));
            }
        }
    }

    fn open_preview(&mut self) {
        self.start_cleanup_after_sample = false;
        self.page = Page::Preview;
        self.detail_scroll = 0;
        self.preview_plan = None;
        self.preview_error = None;
        self.preview_sample = None;
        self.preview_refresh_queued = false;
        self.reload_policy();
        let Some(snapshot) = self.snapshot.as_ref().cloned() else {
            return;
        };
        self.preview_sample = Some(since_sample(&snapshot));
        let fallback = || {
            evaluate(
                &snapshot,
                &self.policy,
                &PolicyContext {
                    state_valid: false,
                    a1_enabled: false,
                },
            )
        };
        if self.policy_error.is_some() {
            self.preview_error = self.policy_error.clone();
            self.preview_plan = Some(fallback());
            return;
        }
        let Some(store) = &self.store else {
            self.preview_error = Some("规则存储不可用".into());
            self.preview_plan = Some(fallback());
            return;
        };
        match crate::preview::prepare(store, &snapshot) {
            Ok(plan) => {
                self.preview_plan = Some(plan);
                self.reload_policy();
            }
            Err(error) => {
                self.reload_policy();
                self.preview_error = Some(safe_text(&error));
                self.preview_plan = Some(evaluate(
                    &snapshot,
                    &self.policy,
                    &PolicyContext {
                        state_valid: false,
                        a1_enabled: false,
                    },
                ));
                self.notice = Some(format!(
                    "预演未完成：{}。本阶段不发送退出请求。",
                    safe_text(&error)
                ));
            }
        }
    }

    fn groups(&self) -> Vec<&OccupancyGroup> {
        let mut groups: Vec<_> = self
            .snapshot
            .as_ref()
            .into_iter()
            .flat_map(|snapshot| &snapshot.groups)
            .filter(|group| match self.filter {
                Filter::Pending => self
                    .entry(&group.id)
                    .is_some_and(|entry| entry.disposition == Disposition::Pending),
                Filter::Ai => self.group_has_development_label(group),
                _ => self.filter.accepts(group.category),
            })
            .collect();
        groups.sort_by(|left, right| match self.sort {
            // Separate metric kinds before comparing amounts. Missing values stay last.
            Sort::Memory => left
                .memory_bytes
                .value
                .is_none()
                .cmp(&right.memory_bytes.value.is_none())
                .then_with(|| left.metric_kind.cmp(&right.metric_kind))
                .then_with(|| right.memory_bytes.value.cmp(&left.memory_bytes.value))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.id.cmp(&right.id)),
            Sort::Name => left
                .name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id)),
        });
        groups
    }

    fn reconcile_selection(&mut self) {
        let ids: Vec<_> = self
            .groups()
            .into_iter()
            .map(|group| group.id.clone())
            .collect();
        let index = self
            .selected_group
            .as_ref()
            .and_then(|id| ids.iter().position(|other| other == id));
        let index = index.or_else(|| (!ids.is_empty()).then_some(0));
        self.table.select(index);
        self.selected_group = index.map(|index| ids[index].clone());
    }

    fn received(&mut self, result: Result<Snapshot, String>) {
        self.loading = false;
        // Throttle failed attempts as well; a stale snapshot must not cause a busy retry loop.
        self.sampled = Some(Instant::now());
        match result {
            Ok(snapshot) => {
                // Navigation can retire the command's initial intent. A response
                // alone never authorizes starting cleanup from another page.
                if self.page != Page::Home {
                    self.start_cleanup_after_sample = false;
                }
                let clean_snapshot = self.start_cleanup_after_sample.then(|| snapshot.clone());
                self.snapshot = Some(snapshot);
                self.error = None;
                self.evaluate_policy();
                if let Some(snapshot) = clean_snapshot {
                    self.start_cleanup_after_sample = false;
                    self.start_cleanup(snapshot, CleanupMode::Automatic);
                } else if self.page == Page::Preview
                    && self.preview_plan.is_none()
                    && self.preview_error.is_none()
                    && !self.preview_refresh_queued
                {
                    self.open_preview();
                }
                self.reconcile_selection();
            }
            Err(error) => {
                self.error = Some(safe_text(&error));
                if self.start_cleanup_after_sample {
                    self.start_cleanup_after_sample = false;
                    self.notice = Some(format!(
                        "清理未开始：采样失败：{}；没有发送退出请求。",
                        safe_text(&error)
                    ));
                }
                if self.page == Page::Preview
                    && self.preview_plan.is_none()
                    && !self.preview_refresh_queued
                {
                    self.preview_error = Some(format!("采样失败：{}", safe_text(&error)));
                }
            }
        }
    }

    fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.selected_group = None;
        self.reconcile_selection();
    }

    fn move_row(&mut self, down: bool) {
        let ids: Vec<_> = self
            .groups()
            .into_iter()
            .map(|group| group.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let current = self.table.selected().unwrap_or(0).min(ids.len() - 1);
        let next = if down {
            (current + 1).min(ids.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.table.select(Some(next));
        self.selected_group = Some(ids[next].clone());
    }

    fn handle(&mut self, key: KeyEvent) -> InputResult {
        if key.kind == KeyEventKind::Release {
            return InputResult::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.cleanup.is_some() {
                self.cancel_cleanup();
                return InputResult::Continue;
            }
            return InputResult::Quit(true);
        }
        if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') {
            self.cancel_cleanup();
            return InputResult::Quit(false);
        }
        if self.cleanup.is_some() {
            if key.code == KeyCode::Esc {
                self.cancel_cleanup();
            }
            return InputResult::Continue;
        }
        if self.notice.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.notice = None;
            }
            return InputResult::Continue;
        }
        if self.confirmation.is_some() {
            match key.code {
                KeyCode::Esc => self.confirmation = None,
                KeyCode::Enter => self.confirm_change(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.confirmation_scroll = self.confirmation_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.confirmation_scroll = self.confirmation_scroll.saturating_add(1)
                }
                KeyCode::PageUp => {
                    self.confirmation_scroll = self.confirmation_scroll.saturating_sub(10)
                }
                KeyCode::PageDown => {
                    self.confirmation_scroll = self.confirmation_scroll.saturating_add(10)
                }
                _ => {}
            }
            return InputResult::Continue;
        }
        let previous_page = self.page.clone();
        match key.code {
            KeyCode::Char('r' | 'R') => {
                if self.page == Page::History || matches!(self.page, Page::HistoryDetail(_)) {
                    self.reload_history();
                    return InputResult::Continue;
                }
                if self.page == Page::Results {
                    return InputResult::Continue;
                }
                self.reload_policy();
                if self.page == Page::Settings || matches!(self.page, Page::RuleDetail(_)) {
                    return InputResult::Continue;
                }
                if self.page == Page::Preview {
                    // Retire the old plan immediately. If an earlier request is still
                    // running, its response is not the explicitly requested new sample.
                    self.preview_plan = None;
                    self.preview_error = None;
                    self.preview_sample = None;
                    self.preview_refresh_queued = self.loading;
                }
                return InputResult::Refresh;
            }
            KeyCode::Char('s' | 'S') => {
                self.start_cleanup_after_sample = false;
                self.reload_policy();
                self.page = Page::Settings;
                self.detail_scroll = 0;
            }
            KeyCode::Esc if self.start_cleanup_after_sample => {
                self.start_cleanup_after_sample = false;
                self.notice = Some("已取消清理准备，没有发送退出请求。".into());
            }
            KeyCode::Esc => match self.page {
                Page::Detail(_) => {
                    self.page = Page::Resources;
                    self.detail_scroll = 0;
                }
                Page::Resources => self.page = Page::Home,
                Page::Settings
                | Page::Preview
                | Page::Results
                | Page::History
                | Page::Executing => self.page = Page::Home,
                Page::HistoryDetail(_) => {
                    self.page = Page::History;
                    self.detail_scroll = 0;
                }
                Page::RuleDetail(_) => {
                    self.page = Page::Settings;
                    self.detail_scroll = 0;
                }
                Page::Home => {}
            },
            _ => match &self.page {
                Page::Home => {
                    let mut index = self.menu.selected().unwrap_or(2);
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => index = index.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => index = (index + 1).min(3),
                        KeyCode::Char(value @ '1'..='4') => index = (value as u8 - b'1') as usize,
                        KeyCode::Enter => match index {
                            0 => self.start_automatic_cleanup(),
                            1 => {
                                self.set_filter(Filter::Pending);
                                self.page = Page::Resources;
                            }
                            2 => {
                                self.set_filter(Filter::All);
                                self.page = Page::Resources;
                            }
                            _ => self.open_history(),
                        },
                        KeyCode::Char('p' | 'P') => self.open_preview(),
                        _ => {}
                    }
                    self.menu.select(Some(index));
                }
                Page::Resources => match key.code {
                    KeyCode::Char('p' | 'P') => self.open_preview(),
                    KeyCode::Up | KeyCode::Char('k') => self.move_row(false),
                    KeyCode::Down | KeyCode::Char('j') => self.move_row(true),
                    KeyCode::Tab | KeyCode::Right => {
                        self.set_filter(Filter::ALL[(self.filter.index() + 1) % 5])
                    }
                    KeyCode::BackTab | KeyCode::Left => {
                        self.set_filter(Filter::ALL[(self.filter.index() + 4) % 5])
                    }
                    KeyCode::Char(value @ '1'..='5') => {
                        self.set_filter(Filter::ALL[(value as u8 - b'1') as usize])
                    }
                    KeyCode::Char('o' | 'O') => {
                        self.sort = if self.sort == Sort::Memory {
                            Sort::Name
                        } else {
                            Sort::Memory
                        };
                        self.reconcile_selection();
                    }
                    KeyCode::Enter => {
                        if let Some(id) = &self.selected_group {
                            self.page = Page::Detail(id.clone());
                            self.detail_scroll = 0;
                        }
                    }
                    _ => {}
                },
                Page::Detail(id) => match key.code {
                    KeyCode::Char('a' | 'A') => self.begin_add(&id.clone(), RuleAction::Allow),
                    KeyCode::Char('p' | 'P') => self.begin_add(&id.clone(), RuleAction::Protect),
                    KeyCode::Char('e' | 'E') => self.begin_exit(&id.clone()),
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.detail_scroll = self.detail_scroll.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.detail_scroll = self.detail_scroll.saturating_add(1)
                    }
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                    KeyCode::Home => self.detail_scroll = 0,
                    _ => {}
                },
                Page::Settings => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        let next = self.rules.selected().unwrap_or(0).saturating_sub(1);
                        self.rules
                            .select((!self.policy.rules.is_empty()).then_some(next));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let next = (self.rules.selected().unwrap_or(0) + 1)
                            .min(self.policy.rules.len().saturating_sub(1));
                        self.rules
                            .select((!self.policy.rules.is_empty()).then_some(next));
                    }
                    KeyCode::Char('d' | 'D') => self.begin_remove(),
                    KeyCode::Enter => {
                        if let Some(rule) = self
                            .rules
                            .selected()
                            .and_then(|index| self.policy.rules.get(index))
                        {
                            self.page = Page::RuleDetail(rule.id.clone());
                            self.detail_scroll = 0;
                        }
                    }
                    _ => {}
                },
                Page::History => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        let index = self
                            .history_selection
                            .selected()
                            .unwrap_or(0)
                            .saturating_sub(1);
                        self.history_selection
                            .select((!self.history.is_empty()).then_some(index));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let index = (self.history_selection.selected().unwrap_or(0) + 1)
                            .min(self.history.len().saturating_sub(1));
                        self.history_selection
                            .select((!self.history.is_empty()).then_some(index));
                    }
                    KeyCode::Enter => {
                        if let Some(item) = self
                            .history_selection
                            .selected()
                            .and_then(|index| self.history.get(index))
                        {
                            self.page = Page::HistoryDetail(item.run_id.clone());
                            self.detail_scroll = 0;
                        }
                    }
                    _ => {}
                },
                Page::RuleDetail(_) | Page::Preview | Page::Results | Page::HistoryDetail(_) => {
                    match key.code {
                        KeyCode::Char('d' | 'D') if matches!(self.page, Page::RuleDetail(_)) => {
                            self.begin_remove()
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.detail_scroll = self.detail_scroll.saturating_sub(1)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.detail_scroll = self.detail_scroll.saturating_add(1)
                        }
                        KeyCode::PageUp => {
                            self.detail_scroll = self.detail_scroll.saturating_sub(10)
                        }
                        KeyCode::PageDown => {
                            self.detail_scroll = self.detail_scroll.saturating_add(10)
                        }
                        KeyCode::Home => self.detail_scroll = 0,
                        _ => {}
                    }
                }
                Page::Executing => {}
            },
        }
        if self.page != previous_page {
            self.start_cleanup_after_sample = false;
        }
        InputResult::Continue
    }

    fn should_refresh(&self, interval: Duration) -> bool {
        !self.loading
            && self.page == Page::Resources
            && self
                .sampled
                .is_some_and(|sampled| sampled.elapsed() >= interval)
    }

    fn take_queued_preview_refresh(&mut self) -> bool {
        if self.page == Page::Preview && self.preview_refresh_queued && !self.loading {
            self.preview_refresh_queued = false;
            true
        } else {
            false
        }
    }
}

fn refresh(app: &mut App, worker: &mut Option<Worker>) -> Result<(), String> {
    if worker.is_none() {
        // AppKit initialization must happen on the main thread before moving the collector.
        match Collector::new() {
            Ok(collector) => *worker = Some(Worker::new(collector)),
            Err(error) => {
                app.received(Err(error));
                return Ok(());
            }
        }
    }
    pump_platform_events();
    app.loading = true;
    worker
        .as_ref()
        .unwrap()
        .request
        .send(Request::Refresh)
        .map_err(|_| "采集线程已退出，终端将恢复。".to_string())
}

/// Enter an interactive UI. Only resources/watch refresh periodically; the home page stays idle.
/// Returns `true` for raw Ctrl+C and `false` for Q. External signals restore and exit directly.
pub fn run(watch: bool, interval: Duration) -> Result<bool, String> {
    run_internal(watch, interval, false)
}

/// An explicit `bree clean` command prepares a session after its first real sample.
/// The native session owns all action gates; an empty plan produces an honest result.
pub fn run_clean() -> Result<bool, String> {
    run_internal(false, Duration::from_secs(2), true)
}

fn run_internal(watch: bool, interval: Duration, clean: bool) -> Result<bool, String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("交互界面需要 TTY；请使用 bree status --json 或 bree list --json。".into());
    }
    // With ctrlc's `termination` feature this also handles external SIGTERM/SIGHUP.
    // Raw-mode Ctrl+C remains a key event. The CLI must not install another handler for TUI.
    ctrlc::set_handler(|| exit_for_external_signal())
        .map_err(|error| format!("无法设置终端中断恢复：{error}"))?;
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|error| format!("无法创建终端界面：{error}"))?;
    let mut app = App::new(watch);
    app.start_cleanup_after_sample = clean;
    app.refresh_interval = interval.max(Duration::from_millis(100));
    // Show the skeleton before native initialization or process enumeration.
    terminal
        .draw(|frame| render(frame, &mut app))
        .map_err(|error| error.to_string())?;
    let mut worker = None;
    let refresh_interval = app.refresh_interval;
    let result = run_loop(&mut terminal, &mut app, &mut worker, refresh_interval);
    // Restore the terminal before waiting for an in-flight, read-only snapshot to finish.
    drop(terminal);
    drop(guard);
    // An event/draw error also stops queued actions. Restore the terminal first;
    // then durably classify any sent, unverified request without sending more.
    app.cancel_cleanup();
    drop(worker);
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    worker: &mut Option<Worker>,
    interval: Duration,
) -> Result<bool, String> {
    app.initialize_store();
    refresh(app, worker)?;
    let mut redraw = true;
    loop {
        if let Some(worker) = worker.as_ref() {
            match worker.response.try_recv() {
                Ok(result) => {
                    app.received(result);
                    redraw = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("采集线程意外退出，终端已恢复。".into());
                }
            }
        }
        if app.should_refresh(interval) {
            refresh(app, worker)?;
            redraw = true;
        }
        if app.take_queued_preview_refresh() {
            refresh(app, worker)?;
            redraw = true;
        }
        if app.advance_cleanup() {
            redraw = true;
        }
        if redraw {
            terminal
                .draw(|frame| render(frame, app))
                .map_err(|error| format!("终端绘制失败：{error}"))?;
            redraw = false;
        }
        // Blocking poll avoids a rendering/animation loop while still receiving worker data.
        let timeout = if app.cleanup.is_some() {
            Duration::from_millis(100)
        } else if app.loading {
            Duration::from_millis(50)
        } else if app.page == Page::Resources {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(500)
        };
        if event::poll(timeout).map_err(|error| format!("终端事件读取失败：{error}"))? {
            match event::read().map_err(|error| format!("终端事件读取失败：{error}"))? {
                Event::Key(key) => {
                    match app.handle(key) {
                        InputResult::Quit(cancelled) => return Ok(cancelled),
                        InputResult::Refresh if !app.loading => refresh(app, worker)?,
                        _ => {}
                    }
                    redraw = true;
                }
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }
    }
}

fn category_name(category: Category) -> &'static str {
    match category {
        Category::Application => "应用",
        Category::AiDevelopment => "AI / 开发",
        Category::System => "系统",
        Category::Unknown => "归属未知",
    }
}

fn action_name(action: RuleAction) -> &'static str {
    match action {
        RuleAction::Allow => "允许",
        RuleAction::Protect => "保护",
    }
}

fn disposition_name(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Automatic => "自动候选",
        Disposition::Pending => "待确认",
        Disposition::Protected => "保护",
    }
}

fn scope_lines(scope: &AppScope) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("Bundle ID: {}", safe_text(&scope.bundle_id))),
        Line::from(format!("安装位置: {}", safe_text(&scope.bundle_path))),
        Line::from(format!(
            "主可执行文件: {}",
            safe_text(&scope.executable_path)
        )),
        Line::from("规则只匹配以上确切安装，不使用名称、PID 或通配范围。"),
    ]
}

fn render_scrolled(frame: &mut Frame<'_>, lines: Vec<Line<'static>>, area: Rect, scroll: &mut u16) {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(area.width)
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max);
    frame.render_widget(paragraph.scroll((*scroll, 0)), area);
}

fn render_settings(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("设置 · 允许与保护规则")), chunks[0]);
    let status = match &app.policy_error {
        Some(error) => format!(
            "规则读取失败，当前只读：{}\n禁用保存与撤销，不会覆盖原文件；R 重新读取。",
            safe_text(error)
        ),
        None => format!(
            "{} 条规则 · 修订 {} · 保护覆盖允许\n规则来自用户主动设置。{}",
            app.policy.rules.len(),
            app.policy.revision,
            if cleanup::enabled() {
                "保存规则不会立即退出应用。"
            } else {
                "正常退出能力尚未启用。"
            }
        ),
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    if app.policy.rules.is_empty() {
        frame.render_widget(Paragraph::new("尚无规则，自动候选可以为零。\n从对象详情按 A 设置允许、P 设置保护，并确认确切安装范围。\n规则设置不会立即退出应用。").wrap(Wrap { trim: false }), chunks[2]);
    } else {
        let rules: Vec<_> = app
            .policy
            .rules
            .iter()
            .map(|rule| {
                ListItem::new(format!(
                    "{} · {} · {}{}",
                    action_name(rule.action),
                    safe_text(&rule.scope.bundle_id),
                    safe_text(&rule.scope.bundle_path),
                    if rule.enabled { "" } else { " · 未启用" }
                ))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(rules).highlight_symbol("> ").highlight_style(
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            chunks[2],
            &mut app.rules,
        );
    }
    frame.render_widget(Paragraph::new("↑↓ 选择  Enter 查看范围  D 移除选定规则\nR 重新读取  Esc 首页  Q 退出\n撤销只移除选定规则；不会恢复整份旧配置。").style(Style::default().add_modifier(Modifier::DIM)), chunks[3]);
}

fn render_rule_detail(frame: &mut Frame<'_>, app: &mut App, id: &str, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("规则范围")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(rule) = app.policy.rules.iter().find(|rule| rule.id == id) {
        lines.push(Line::styled(
            format!("{}规则 · {}", action_name(rule.action), safe_text(&rule.id)),
            Style::default().fg(ACCENT),
        ));
        lines.extend(scope_lines(&rule.scope));
        lines.push(Line::from(format!(
            "来源 {} · 状态 {} · 创建 UTC 毫秒 {}",
            safe_text(&rule.source),
            if rule.enabled { "启用" } else { "停用" },
            rule.created_at_unix_ms
        )));
        lines.push(Line::from("保存或查看规则不会立即触发退出请求。"));
    } else {
        lines.push(Line::from("此规则已移除；Esc 返回设置。"));
    }
    if let Some(error) = &app.policy_error {
        lines.push(Line::from(format!(
            "规则读取失败，旧规则仅供查看：{}",
            safe_text(error)
        )));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("D 移除这条规则  ↑↓/PgDn 滚动\nR 重新读取  Esc 设置  Q 退出")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_preview(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(title("清理预演 · 不发送退出请求")),
        chunks[0],
    );
    let mut lines = Vec::new();
    if let Some(plan) = &app.preview_plan {
        lines.push(Line::styled(
            format!(
                "自动候选 {} · 待确认 {} · 保护 {}",
                plan.automatic_count, plan.pending_count, plan.protected_count
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(if cleanup::enabled() {
            "此页不发送退出请求；实际执行前仍会重验实例、保护与前台状态。"
        } else {
            "A1 退出能力尚未开放；允许规则只参与分类。自动候选为零是合法结果。"
        }));
        lines.push(Line::from(format!(
            "计划 {} · 规则修订 {} · 生成 UTC 毫秒 {}",
            safe_text(&plan.plan_id),
            plan.rule_revision,
            plan.created_at_unix_ms
        )));
        if let Some(sample) = &app.preview_sample {
            lines.push(Line::from(sample.clone()));
        }
        lines.push(Line::from(
            "这是按以上采样和规则冻结的预演；R 重新采样，资源变化不自动更新此页。",
        ));
        if plan.rule_revision != app.policy.revision {
            lines.push(Line::styled(
                "规则已变更；当前预演为旧修订，按 R 重新预演。",
                Style::default().fg(ACCENT),
            ));
        }
        if app.error.is_some() {
            lines.push(Line::styled(
                "刷新失败；以上为旧采样。",
                Style::default().fg(ACCENT),
            ));
        }
        if let Some(error) = &app.policy_error {
            lines.push(Line::styled(
                format!("规则不可读取，保持保护与只读：{}", safe_text(error)),
                Style::default().fg(ACCENT),
            ));
        }
        if let Some(error) = &app.preview_error {
            lines.push(Line::styled(
                format!(
                    "预演未完成：{}；以下为保守解释，未记录成功预演。",
                    safe_text(error)
                ),
                Style::default().fg(ACCENT),
            ));
        }
        lines.push(Line::from(""));
        for entry in &plan.entries {
            lines.push(Line::styled(
                format!(
                    "{} · {}",
                    disposition_name(entry.disposition),
                    safe_text(&entry.name)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for reason in &entry.reasons {
                lines.push(Line::from(format!("  · {}", safe_text(reason))));
            }
            lines.push(Line::from(""));
        }
    } else if let Some(error) = &app.preview_error {
        lines.push(Line::styled(
            format!("预演未完成：{}。R 重新采样后重试。", safe_text(error)),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::from(
            "没有新计划，也未记录成功预演；不会发送退出请求。",
        ));
    } else if app.loading || app.preview_refresh_queued {
        lines.push(Line::from(
            "正在采样，完成后生成一次新预演；不会发送退出请求。",
        ));
    } else {
        lines.push(Line::from(
            "数据尚未读取，无法生成预演。R 刷新后重试；不会发送退出请求。",
        ));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("↑↓/PgDn 滚动  R 重新采样并预演\nS 设置  Esc 首页  Q 退出 · 只读计划")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn result_lines(result: &CleanupResult) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!(
            "运行 {} · 规则修订 {}",
            safe_text(&result.run_id),
            result.rule_revision
        )),
        Line::from(format!("计划 {}", safe_text(&result.plan_id))),
    ];
    if result.cancelled {
        lines.push(Line::styled(
            "本次已取消；已发送的请求无法撤回，未核验状态会明确保留。",
            Style::default().fg(ACCENT),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "应用退出结果",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if result.targets.is_empty() {
        lines.push(Line::from("没有处理目标，没有发送退出请求。"));
    }
    for target in &result.targets {
        lines.push(Line::styled(
            format!("{} · {}", target.outcome.text(), safe_text(&target.name)),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!(
            "{} · 观察 {} ms",
            if target.request_sent {
                "已发送正常退出请求"
            } else {
                "未发送请求"
            },
            target.observed_ms
        )));
        if let Some(identity) = &target.identity {
            lines.push(Line::from(format!(
                "确切实例 {}",
                safe_text(&identity.object_id())
            )));
        }
        lines.push(Line::from(format!("依据：{}", safe_text(&target.reason))));
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        "系统资源观察",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(format!(
        "操作前：已用 {} · 压力 {} · 压缩 {} · 交换 {}",
        metric_bytes(&result.resource_before.used_bytes),
        pressure_text(&result.resource_before.pressure),
        metric_bytes(&result.resource_before.compressed_bytes),
        metric_bytes(&result.resource_before.swap_used_bytes)
    )));
    if let Some(after) = &result.resource_after {
        lines.push(Line::from(format!(
            "操作后：已用 {} · 压力 {} · 压缩 {} · 交换 {}",
            metric_bytes(&after.used_bytes),
            pressure_text(&after.pressure),
            metric_bytes(&after.compressed_bytes),
            metric_bytes(&after.swap_used_bytes)
        )));
    } else {
        lines.push(Line::from("操作后：未取得资源采样，不能判断改善情况。"));
    }
    lines.push(Line::from(safe_text(&result.resource_observation)));
    lines.push(Line::from(
        "内存或压力没有改善也是合法结果；系统变化不等于本次操作保证释放的内存。",
    ));
    for error in &result.errors {
        lines.push(Line::styled(
            format!("记录 / 观察错误：{}", safe_text(error)),
            Style::default().fg(ACCENT),
        ));
    }
    lines
}

fn render_executing(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("正常退出 · 执行与观察")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(session) = &app.cleanup {
        lines.push(Line::styled(
            safe_text(session.progress()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!(
            "冻结计划 {} · 规则修订 {}",
            safe_text(&session.plan().plan_id),
            session.plan().rule_revision
        )));
        lines.push(Line::from(
            "每个请求前重新核验确切实例、安装、保护与前台状态。",
        ));
        lines.push(Line::from(
            "观察最多 15 秒；拒绝或仍在运行不会触发强制结束。",
        ));
        lines.push(Line::from(""));
        for target in session.targets() {
            lines.push(Line::styled(
                format!("{} · {}", safe_text(&target.name), target.outcome.text()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::from(format!(
                "{} · {}",
                if target.request_sent {
                    "请求已发送"
                } else {
                    "未发送请求"
                },
                safe_text(&target.reason)
            )));
            lines.push(Line::from(""));
        }
    } else {
        lines.push(Line::from("执行会话已结束；Esc 返回首页。"));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("Ctrl+C / Esc 取消当次，停止后续请求\nQ 先取消再退出 · 不会重放动作")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_results(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("处理结果")), chunks[0]);
    let lines = app
        .cleanup_result
        .as_ref()
        .map(result_lines)
        .unwrap_or_else(|| vec![Line::from("没有完成结果；可在处理记录中查看未完成的运行。")]);
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new(
            "↑↓ / PgUp / PgDn 滚动\nEsc 首页  S 设置  Q 退出 · 退出状态与资源观察分别记录",
        )
        .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn history_status(status: &str) -> String {
    match status {
        "finished" => "已完成".into(),
        "cancelled" => "已取消".into(),
        "unfinished" => "未完成".into(),
        other => safe_text(other),
    }
}

fn render_history(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("处理记录 · 最近运行")), chunks[0]);
    let status = match &app.history_error {
        Some(error) => format!(
            "记录读取失败：{}\n以下缓存记录可能过期；R 重试，不会重放动作。",
            safe_text(error)
        ),
        None => format!(
            "{} 条运行记录 · 默认保留 7 天、总量上限 10 MiB\n完成与未完成分别显示；查看记录不会重新执行。",
            app.history.len()
        ),
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    if app.history.is_empty() {
        frame.render_widget(
            Paragraph::new("暂无清理运行记录。规则与只读预演日志不会被当作真实退出结果。")
                .wrap(Wrap { trim: false }),
            chunks[2],
        );
    } else {
        let items: Vec<_> = app
            .history
            .iter()
            .map(|item| {
                ListItem::new(vec![
                    Line::from(format!(
                        "{} · {} · UTC 毫秒 {}",
                        history_status(&item.status),
                        if item.result.is_some() {
                            "已记录结果"
                        } else {
                            "未完成 / 无最终结果"
                        },
                        item.timestamp_unix_ms
                    )),
                    Line::from(format!(
                        "{} · {}",
                        safe_text(&item.run_id),
                        safe_text(&item.message)
                    )),
                ])
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items).highlight_symbol("> ").highlight_style(
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            chunks[2],
            &mut app.history_selection,
        );
    }
    frame.render_widget(
        Paragraph::new(
            "↑↓ 选择  Enter 查看结果  R 重新读取\nEsc 首页  Q 退出 · 未完成运行不会重放",
        )
        .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[3],
    );
}

fn render_history_detail(frame: &mut Frame<'_>, app: &mut App, id: &str, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("运行记录详情")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(item) = app.history.iter().find(|item| item.run_id == id) {
        lines.push(Line::styled(
            format!(
                "{} · UTC 毫秒 {}",
                history_status(&item.status),
                item.timestamp_unix_ms
            ),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::from(safe_text(&item.message)));
        if let Some(result) = &item.result {
            lines.extend(result_lines(result));
        } else {
            lines.push(Line::from(format!(
                "运行 {} 没有最终结果。",
                safe_text(&item.run_id)
            )));
            lines.push(Line::from(
                "无法确认上次已发送请求的最终退出状态；Bree 不会重放这次运行。",
            ));
        }
    } else {
        lines.push(Line::from("此运行已不在最近记录中；Esc 返回列表。"));
    }
    if let Some(error) = &app.history_error {
        lines.push(Line::styled(
            format!("读取失败，当前为缓存记录：{}", safe_text(error)),
            Style::default().fg(ACCENT),
        ));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("↑↓ / PgUp / PgDn 滚动  R 重新读取\nEsc 返回记录  Q 退出 · 只查看，不重放")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_confirmation(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(confirmation) = &app.confirmation else {
        return;
    };
    let mut lines = Vec::new();
    let heading = match confirmation {
        RuleConfirmation::Add {
            action,
            scope,
            name,
        } => {
            lines.push(Line::from(format!("对象：{}", safe_text(name))));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "这是持续规则，须主动确认；当前不会发送退出请求。保护规则优先。",
            ));
            format!(" 新建{}规则 ", action_name(*action))
        }
        RuleConfirmation::Remove { id, action, scope } => {
            lines.push(Line::from(format!(
                "移除{}规则 {}",
                action_name(*action),
                safe_text(id)
            )));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "只移除这一条规则，不恢复旧配置，也不会退出应用。",
            ));
            " 撤销选定规则 ".into()
        }
        RuleConfirmation::Exit {
            name,
            scope,
            identity,
            ..
        } => {
            lines.push(Line::from(format!("应用：{}", safe_text(name))));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(format!(
                "确切实例：{}",
                safe_text(&identity.object_id())
            )));
            lines.push(Line::from(format!(
                "PID {} · 启动 {} 秒 + {} 微秒",
                identity.pid,
                identity
                    .start_seconds
                    .map_or_else(|| "未知".into(), |value| value.to_string()),
                identity
                    .start_microseconds
                    .map_or_else(|| "未知".into(), |value| value.to_string())
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("仅对这个确切实例请求一次正常退出。"));
            lines.push(Line::from("单次操作不新增允许规则。"));
            lines.push(Line::from("拒绝或超时不会强退。"));
            lines.push(Line::from("执行前重新核验；替代实例会跳过。"));
            " 单次正常退出确认 ".into()
        }
    };
    let width = area.width.min(88);
    let height = area.height.min(18);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(heading)
        .style(Style::default().fg(FOREGROUND).bg(BACKGROUND))
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    render_scrolled(frame, lines, chunks[0], &mut app.confirmation_scroll);
    frame.render_widget(
        Paragraph::new(if matches!(confirmation, RuleConfirmation::Exit { .. }) {
            "Enter 请求正常退出  Esc 取消\n↑↓ / PgDn 查看完整安装与实例范围"
        } else {
            "Enter 确认保存  Esc 取消\n↑↓ / PgDn 查看完整安装范围"
        })
        .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[1],
    );
}

fn since_sample(snapshot: &Snapshot) -> String {
    // An absolute time stays true while the home screen deliberately remains static.
    let seconds = snapshot.sampled_at_unix_ms / 1000;
    let day_seconds = seconds % 86_400;
    format!(
        "采样 UTC {:02}:{:02}:{:02} · {} ms",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60,
        snapshot.collected_in_ms
    )
}

fn title(value: &str) -> Line<'_> {
    Line::from(vec![
        Span::styled(
            " bree ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            value,
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ),
    ])
}

fn brand_header(expanded: bool) -> Vec<Line<'static>> {
    if !expanded {
        return vec![
            Line::from(vec![
                Span::styled(" ( -.- ) ", Style::default().fg(ACCENT)),
                Span::styled("bree", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    " · 规则与预演",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]),
            Line::from("看清内存占用，让每个选择都有依据。"),
        ];
    }
    // Plain terminal cells: the curled, sleeping mascot and lowercase wordmark
    // need neither a patched font nor an image protocol.
    let mascot = [
        "      .-.._  z  ",
        "     ( -.- )__  ",
        "   .-'     `-.  ",
        "  (   .----.  ) ",
        "   `-(_  __)-'  ",
        "      `----'    ",
    ];
    let wordmark = [
        "  ▄             ",
        "  █▄▄▄  ▄ ▄▄  ▄▄▄▄  ▄▄▄▄",
        "  █  █  █▀   █▄▄▄█ █▄▄▄█",
        "  █▄▄█  █     ▀▄▄▄  ▀▄▄▄",
        "",
        "  bree · 规则与预演",
    ];
    let mut lines: Vec<_> = mascot
        .into_iter()
        .zip(wordmark)
        .map(|(mascot, wordmark)| {
            Line::from(vec![
                Span::styled(mascot, Style::default().fg(ACCENT)),
                Span::styled(wordmark, Style::default().add_modifier(Modifier::BOLD)),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from("看清内存占用，让每个选择都有依据。"));
    lines
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(FOREGROUND).bg(BACKGROUND)),
        area,
    );
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(if app.cleanup.is_some() { "bree · 正常退出观察\n请调整终端至至少 48 列 × 16 行。\nCtrl+C 取消当次，Q 先取消再退出" } else { "bree · 内存检查\n请调整终端至至少 48 列 × 16 行。\nQ 或 Ctrl+C 退出" })
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(ACCENT)),
            area,
        );
        return;
    }
    let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
    match app.page.clone() {
        Page::Home => render_home(frame, app, inner),
        Page::Resources => render_resources(frame, app, inner),
        Page::Detail(id) => render_detail(frame, app, &id, inner),
        Page::Settings => render_settings(frame, app, inner),
        Page::RuleDetail(id) => render_rule_detail(frame, app, &id, inner),
        Page::Preview => render_preview(frame, app, inner),
        Page::Executing => render_executing(frame, app, inner),
        Page::Results => render_results(frame, app, inner),
        Page::History => render_history(frame, app, inner),
        Page::HistoryDetail(id) => render_history_detail(frame, app, &id, inner),
    }
    if app.confirmation.is_some() {
        render_confirmation(frame, app, inner);
    }
    if let Some(notice) = &app.notice {
        let width = inner.width.min(72);
        let height = inner.height.min(9);
        let popup = Rect::new(
            inner.x + (inner.width - width) / 2,
            inner.y + (inner.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(format!("{}\n\nEnter / Esc 返回", safe_text(notice)))
                .style(Style::default().fg(FOREGROUND).bg(BACKGROUND))
                .block(
                    Block::bordered()
                        .title(" bree · 提示 ")
                        .border_style(Style::default().fg(ACCENT)),
                )
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn render_home(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let expanded = area.width >= 76 && area.height >= 22;
    let summary_height = if app.snapshot.is_some()
        && app.policy_error.is_some()
        && (app.error.is_some() || app.loading)
    {
        6
    } else {
        5
    };
    let chunks = Layout::vertical([
        Constraint::Length(if expanded { 8 } else { 2 }),
        Constraint::Length(summary_height),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(brand_header(expanded)), chunks[0]);
    frame.render_widget(
        Paragraph::new(summary(app)).wrap(Wrap { trim: false }),
        chunks[1],
    );
    let pending = app.plan.as_ref().map_or_else(
        || "分析中".into(),
        |plan| format!("{} 项 · 查看理由", plan.pending_count),
    );
    let automatic = app.plan.as_ref().map_or(0, |plan| plan.automatic_count);
    let entries = [
        if !cleanup::enabled() {
            "1. 一键清理       正常退出能力未启用".into()
        } else if automatic == 0 {
            "1. 一键清理       暂无可处理对象".into()
        } else {
            format!("1. 一键清理       {automatic} 个允许的确切实例")
        },
        format!("2. 待确认项目     {pending}"),
        "3. 全部内存占用   按应用分组查看".into(),
        "4. 处理记录       查看完成与未完成记录".into(),
    ];
    let list = List::new(entries.map(ListItem::new))
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(FOREGROUND)
                .bg(SELECTED)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        );
    frame.render_stateful_widget(list, chunks[2], &mut app.menu);
    frame.render_widget(
        Paragraph::new("↑↓ / 1–4 选择  Enter 进入  R 刷新\nP 只读预演  S 设置  Q 退出 · 首页静置")
            .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[3],
    );
}

fn summary(app: &App) -> Text<'static> {
    let Some(snapshot) = &app.snapshot else {
        let message = if let Some(error) = &app.error {
            format!(
                "读取失败：{}\nR 重试；未知数据不作为零占用。",
                safe_text(error)
            )
        } else {
            format!(
                "分析中…\n正在读取真实内存数据，{}。",
                if cleanup::enabled() {
                    "清理只处理明确允许的普通应用"
                } else {
                    "退出操作尚未启用"
                }
            )
        };
        return Text::from(message);
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "内存压力：",
                Style::default().fg(MUTED).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                pressure_text(&snapshot.system.pressure),
                Style::default().fg(ACCENT),
            ),
        ]),
        Line::from(format!(
            "已用 {} / 总量 {}",
            metric_bytes(&snapshot.system.used_bytes),
            metric_bytes(&snapshot.system.total_bytes)
        )),
        Line::from(format!(
            "压缩 {} · 交换空间 {}",
            metric_bytes(&snapshot.system.compressed_bytes),
            metric_bytes(&snapshot.system.swap_used_bytes)
        )),
        Line::from(format!(
            "{} · 覆盖 {}/{} 进程",
            since_sample(snapshot),
            snapshot.coverage.readable_memory_processes,
            snapshot.coverage.enumerated_processes
        )),
    ];
    if let Some(error) = &app.error {
        lines.push(Line::styled(
            format!("刷新失败：{}；以上为旧数据", safe_text(error)),
            Style::default().fg(ACCENT),
        ));
    } else if app.loading {
        lines.push(Line::styled(
            "刷新中 · 以上为上次采样",
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ));
    }
    if app.policy_error.is_some() {
        lines.push(Line::styled(
            "规则不可读取 · S 查看原因 · 保持只读",
            Style::default().fg(ACCENT),
        ));
    }
    Text::from(lines)
}

fn render_resources(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let compact = area.width < 76;
    let wide = area.width >= 118;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("全部内存占用")), chunks[0]);
    let status = if let Some(snapshot) = &app.snapshot {
        let suffix = if app.error.is_some() {
            " · 刷新失败，旧数据"
        } else if app.loading {
            " · 刷新中"
        } else {
            ""
        };
        format!(
            "压力 {} · 已用 {} / {}\n{}{}",
            pressure_text(&snapshot.system.pressure),
            metric_bytes(&snapshot.system.used_bytes),
            metric_bytes(&snapshot.system.total_bytes),
            since_sample(snapshot),
            suffix
        )
    } else if let Some(error) = &app.error {
        format!("读取失败：{} · R 重试", safe_text(error))
    } else {
        "分析中… 正在读取真实进程占用".into()
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    let tabs = Tabs::new(["全部", "应用", "AI/开发", "系统", "待确认"])
        .select(app.filter.index())
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider(" ")
        .padding("", "");
    frame.render_widget(tabs, chunks[2]);

    let groups = app.groups();
    if groups.is_empty() {
        let message = if app.filter == Filter::Pending {
            "当前预演没有待确认对象。\n保护对象仍可在全部占用中查看分类理由。"
        } else if app.filter == Filter::Ai {
            "本次采样未匹配到可靠的开发工具安装证据。\n尚未覆盖的对象保留在全部占用中；不代表没有 AI 任务。"
        } else if app.loading && app.snapshot.is_none() {
            "分析中…"
        } else if app.error.is_some() && app.snapshot.is_none() {
            "读取失败；按 R 重试。"
        } else {
            "此筛选下没有可显示的对象。"
        };
        frame.render_widget(
            Paragraph::new(message)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
            chunks[3],
        );
    } else {
        let rows: Vec<_> = groups
            .iter()
            .map(|group| {
                let mut values = if compact {
                    vec![
                        safe_text(&group.name),
                        metric_bytes(&group.memory_bytes),
                        group.process_ids.len().to_string(),
                        app.entry(&group.id)
                            .map_or("未分类", |entry| disposition_name(entry.disposition))
                            .into(),
                    ]
                } else {
                    vec![
                        safe_text(&group.name),
                        metric_bytes(&group.memory_bytes),
                        group.process_ids.len().to_string(),
                        category_name(group.category).into(),
                        safe_text(&group.metric_kind),
                        app.entry(&group.id)
                            .map_or("未分类", |entry| disposition_name(entry.disposition))
                            .into(),
                    ]
                };
                if wide {
                    values.push(
                        app.entry(&group.id)
                            .and_then(|entry| entry.reasons.first())
                            .map(|reason| safe_text(reason))
                            .unwrap_or_else(|| "等待分类".into()),
                    );
                }
                Row::new(values)
            })
            .collect();
        let (mut header, mut widths) = if compact {
            (
                vec!["名称", "占用", "实例", "预演"],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(10),
                    Constraint::Length(5),
                    Constraint::Length(8),
                ],
            )
        } else {
            (
                vec!["名称", "占用", "实例", "归属", "指标", "预演"],
                vec![
                    Constraint::Min(16),
                    Constraint::Length(12),
                    Constraint::Length(5),
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(8),
                ],
            )
        };
        if wide {
            header.push("依据");
            widths.push(Constraint::Min(26));
        }
        let table = Table::new(rows, widths)
            .header(Row::new(header).style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)))
            .row_highlight_style(
                Style::default()
                    .fg(FOREGROUND)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
            .highlight_symbol("> ");
        frame.render_stateful_widget(table, chunks[3], &mut app.table);
    }
    let footer = format!(
        "↑↓ 选择  Enter 详情与完整理由  Tab 筛选  O:{}\nR 刷新  S 设置  Esc 首页  Q 退出 · {:.1}s 更新\n{}",
        if app.sort == Sort::Memory {
            "内存"
        } else {
            "名称"
        },
        app.refresh_interval.as_secs_f64(),
        app.selected_group
            .as_ref()
            .and_then(|id| app.entry(id))
            .and_then(|entry| entry.reasons.first())
            .map(|reason| format!("理由：{}", safe_text(reason)))
            .unwrap_or_else(|| "组内指标合计；不同指标分榜，非可回收内存。".into())
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[4],
    );
}

fn process_detail(process: &ProcessInfo) -> Vec<Line<'static>> {
    let protection = if process.protection_reasons.is_empty() {
        "未检测到采集层保护标记；以上方预演分类及执行前重验为准".into()
    } else {
        process
            .protection_reasons
            .iter()
            .map(|reason| safe_text(reason))
            .collect::<Vec<_>>()
            .join("；")
    };
    let attribution = &process.attribution;
    let application = attribution.application.as_ref();
    let mut lines = vec![
        Line::styled(
            format!(
                "{} · PID {}",
                safe_text(&process.name),
                process.identity.pid
            ),
            Style::default().fg(ACCENT),
        ),
        Line::from(format!(
            "占用 {} · 指标 {}",
            metric_bytes(&process.memory_bytes),
            safe_text(&process.metric_kind)
        )),
        Line::from(format!(
            "数据有效性 {:?} · 来源 {}",
            process.memory_bytes.status,
            safe_text(&process.memory_bytes.source)
        )),
        Line::from(format!("实例 {}", safe_text(&process.id))),
        Line::from(format!(
            "身份 {:?} · 启动 {} 秒 + {} 微秒",
            process.identity.status,
            process
                .identity
                .start_seconds
                .map_or_else(|| "未知".into(), |value| value.to_string()),
            process
                .identity
                .start_microseconds
                .map_or_else(|| "未知".into(), |value| value.to_string())
        )),
        Line::from(format!(
            "可执行位置 {}",
            process
                .executable_path
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "— / 不可读取".into())
        )),
        Line::from(format!(
            "归属 {} · 置信度 {}",
            safe_text(&attribution.method),
            safe_text(&attribution.confidence)
        )),
        Line::from(safe_text(&attribution.explanation)),
        Line::from(format!("保护 / 限制 {protection}")),
    ];
    if let Some(application) = application {
        lines.push(Line::from(format!(
            "应用 {} · Bundle {} · 前台 {}",
            safe_text(&application.name),
            application
                .bundle_id
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "未知".into()),
            if application.frontmost { "是" } else { "否" }
        )));
        lines.push(Line::from(format!(
            "应用位置 {}",
            safe_text(&application.bundle_path)
        )));
    }
    if let Some(cpu) = process.cpu_one_core_percent.value {
        lines.push(Line::from(format!(
            "CPU {cpu:.1}%（单核）· 来源 {}",
            safe_text(&process.cpu_one_core_percent.source)
        )));
    } else {
        lines.push(Line::from(format!(
            "CPU — · {}",
            process
                .cpu_one_core_percent
                .reason
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "不可读取".into())
        )));
    }
    if let Some(reason) = &process.memory_bytes.reason {
        lines.push(Line::from(format!("读取说明 {}", safe_text(reason))));
    }
    if let Some(label) = label_process(process) {
        lines.push(Line::styled(
            format!("开发标签 {}", label.label),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::from(format!(
            "依据 {} · 置信度 {}",
            safe_text(&label.evidence),
            label.confidence
        )));
        lines.push(Line::from("标签仅解释归属，不代表任务完成或可以自动停止。"));
    }
    lines.push(Line::from(
        if cleanup::enabled() && process.quit_supported {
            "支持操作：普通主应用可请求正常退出，须通过实例与保护重验。端口：尚未验证。"
        } else {
            "支持操作：只读查看；此实例正常退出未启用。端口：尚未验证。"
        },
    ));
    lines.push(Line::from(""));
    lines
}

fn render_detail(frame: &mut Frame<'_>, app: &mut App, id: &str, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("对象详情 · 只读")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(snapshot) = &app.snapshot {
        if let Some(group) = snapshot.groups.iter().find(|group| group.id == id) {
            lines.push(Line::styled(
                safe_text(&group.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::from(format!(
                "占用 {} · {} 个进程 · {}",
                metric_bytes(&group.memory_bytes),
                group.process_ids.len(),
                category_name(group.category)
            )));
            lines.push(Line::from(format!(
                "指标 {} · 来源 {}",
                safe_text(&group.metric_kind),
                safe_text(&group.memory_bytes.source)
            )));
            lines.push(Line::from(safe_text(&group.explanation)));
            lines.push(Line::from(since_sample(snapshot)));
            if let Some(entry) = app.entry(id) {
                lines.push(Line::styled(
                    format!("预演分类 {}", disposition_name(entry.disposition)),
                    Style::default().fg(ACCENT),
                ));
                for reason in &entry.reasons {
                    lines.push(Line::from(format!("· {}", safe_text(reason))));
                }
                if !entry.matched_rule_ids.is_empty() {
                    lines.push(Line::from(format!(
                        "匹配规则 {}",
                        entry
                            .matched_rule_ids
                            .iter()
                            .map(|id| safe_text(id))
                            .collect::<Vec<_>>()
                            .join("、")
                    )));
                }
            }
            lines.push(Line::from("组内指标合计，非保证释放量。此页面按需刷新。"));
            lines.push(Line::from(""));
            for process_id in &group.process_ids {
                if let Some(process) = snapshot
                    .processes
                    .iter()
                    .find(|process| &process.id == process_id)
                {
                    lines.extend(process_detail(process));
                }
            }
        } else {
            lines.push(Line::from(
                "此对象已不在最近采样中。旧实例不会映射到替代进程；Esc 返回列表。",
            ));
        }
        if let Some(error) = &app.error {
            lines.push(Line::styled(
                format!("刷新失败：{}；当前为旧数据", safe_text(error)),
                Style::default().fg(ACCENT),
            ));
        }
    } else {
        lines.push(Line::from("数据尚未读取；按 R 重试。"));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Clamp against actual wrapped rows on every draw: refresh and resizing can
    // shorten the content even without another navigation key.
    let max_scroll = paragraph
        .line_count(chunks[1].width)
        .saturating_sub(chunks[1].height as usize)
        .min(u16::MAX as usize) as u16;
    app.detail_scroll = app.detail_scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), chunks[1]);
    frame.render_widget(
        Paragraph::new(if cleanup::enabled() {
            "A 允许  P 保护  E 单次退出  ↑↓/PgDn 滚动\nR 刷新  S 设置  Esc 返回  Q 退出"
        } else {
            "A 允许  P 保护  E 退出未启用  ↑↓/PgDn 滚动\nR 刷新  S 设置 Esc 返回  Q 退出"
        })
        .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Application, Attribution, Coverage, Metric, Pressure, ProcessIdentity, SystemMemory,
        Validity,
    };
    use ratatui::backend::TestBackend;

    fn snapshot() -> Snapshot {
        let identity = ProcessIdentity {
            boot_session: "boot".into(),
            pid: 123,
            start_seconds: Some(10),
            start_microseconds: Some(5),
            status: Validity::Ok,
        };
        let id = identity.object_id();
        Snapshot {
            schema_version: 1,
            sampled_at_unix_ms: 1000,
            collected_in_ms: 12,
            system: SystemMemory {
                total_bytes: Metric::ok(16 * 1024 * 1024 * 1024, "test"),
                used_bytes: Metric::ok(8 * 1024 * 1024 * 1024, "test"),
                compressed_bytes: Metric::ok(0, "test"),
                swap_used_bytes: Metric::ok(0, "test"),
                cached_bytes: Metric::ok(1, "test"),
                pressure: Metric::ok(Pressure::Normal, "test"),
                used_definition: "test".into(),
            },
            processes: vec![ProcessInfo {
                id: id.clone(),
                identity,
                parent_pid: Some(1),
                uid: Some(501),
                name: "中文应用".into(),
                executable_path: Some("/Users/example/私有路径/程序".into()),
                memory_bytes: Metric::ok(100 * 1024 * 1024, "test_rss"),
                metric_kind: "rss".into(),
                cpu_one_core_percent: Metric::ok(1.0, "test"),
                category: Category::Application,
                attribution: Attribution {
                    application: None,
                    method: "测试".into(),
                    confidence: "high".into(),
                    explanation: "已验证进程".into(),
                },
                protection_reasons: vec!["前台使用中".into()],
                quit_supported: false,
            }],
            groups: vec![OccupancyGroup {
                id: "group".into(),
                name: "中文应用".into(),
                category: Category::Application,
                memory_bytes: Metric::ok(100 * 1024 * 1024, "test_rss"),
                metric_kind: "rss".into(),
                process_ids: vec![id],
                explanation: "测试组".into(),
            }],
            coverage: Coverage {
                enumerated_processes: 1,
                readable_memory_processes: 1,
                reliable_identity_processes: 1,
                notes: vec![],
            },
            diagnostics: vec![],
        }
    }

    fn screen(app: &mut App, width: u16, height: u16) -> (String, Terminal<TestBackend>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        (text, terminal)
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        // A CJK glyph occupies two cells; its trailing cell is not visible text.
        let mut result = String::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                result.push_str(symbol);
                x += Span::raw(symbol).width().max(1) as u16;
            }
            result.push('\n');
        }
        result
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    struct TestStore(Store);
    impl TestStore {
        fn new() -> Self {
            static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(Store::at(std::env::temp_dir().join(format!(
                "bree-tui-test-{}-{sequence}",
                std::process::id()
            ))))
        }
        fn app(&self) -> App {
            let mut app = App::new(false);
            app.store = Some(self.0.clone());
            app.reload_policy();
            app
        }
    }
    impl Drop for TestStore {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(self.0.root(), std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(self.0.root());
        }
    }

    fn scoped_snapshot(installation: &str) -> Snapshot {
        let mut sample = snapshot();
        let process = &mut sample.processes[0];
        process.uid = Some(unsafe { libc::geteuid() });
        process.executable_path = Some(format!(
            "/Applications/{installation}.app/Contents/MacOS/{installation}"
        ));
        process.protection_reasons.clear();
        process.attribution = Attribution {
            application: Some(Application {
                bundle_id: Some("com.example.fixture".into()),
                bundle_path: format!("/Applications/{installation}.app"),
                name: "中文应用".into(),
                leader_pid: process.identity.pid,
                frontmost: false,
            }),
            method: "appkit_main_application".into(),
            confidence: "high".into(),
            explanation: "AppKit 当前主实例".into(),
        };
        sample.groups[0].id = format!("app:{}", process.id);
        sample
    }

    #[test]
    fn initial_focus_and_read_only_preview_are_safe() {
        let mut app = App::new(false);
        assert_eq!(app.menu.selected(), Some(2));
        let (text, _) = screen(&mut app, 90, 24);
        assert!(text.contains("分析中"));
        assert!(text.contains("> 3. 全部内存占用"));
        app.handle(key(KeyCode::Char('1')));
        assert!(matches!(
            app.handle(key(KeyCode::Enter)),
            InputResult::Continue
        ));
        assert_eq!(app.page, Page::Home);
        assert!(app.cleanup.is_none());
        assert!(app.notice.is_some());
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Char('p')));
        assert_eq!(app.page, Page::Preview);
        let (text, _) = screen(&mut app, 90, 24);
        assert!(text.contains("不发送退出"));
    }

    #[test]
    fn chinese_list_hides_paths_and_detail_explains_identity() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        let (text, _) = screen(&mut app, 100, 30);
        assert!(text.contains("中文应用"));
        assert!(!text.contains("私有路径"));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 120, 40);
        assert!(text.contains("PID 123"));
        assert!(text.contains("私有路径"));
        assert!(text.contains("前台使用中"));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Resources);
    }

    #[test]
    fn resizes_support_compact_layout_and_minimum_size_message() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        let (text, mut terminal) = screen(&mut app, 60, 20);
        assert!(text.contains("中文应用"));
        terminal.backend_mut().resize(30, 10);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("请调整终端"));
        assert!(text.contains("Ctrl+C"));
    }

    #[test]
    fn unknown_memory_sorts_after_measured_zero_and_kinds_do_not_mix() {
        let mut app = App::new(true);
        let mut sample = snapshot();
        let mut zero = sample.groups[0].clone();
        zero.id = "zero".into();
        zero.memory_bytes = Metric::ok(0, "test");
        let mut unknown = zero.clone();
        unknown.id = "unknown".into();
        unknown.memory_bytes = Metric::unavailable(Validity::Denied, "test", "denied");
        let mut different = zero.clone();
        different.id = "footprint".into();
        different.metric_kind = "footprint".into();
        sample.groups.extend([unknown, zero, different]);
        app.received(Ok(sample));
        let ids: Vec<_> = app.groups().iter().map(|group| group.id.as_str()).collect();
        assert_eq!(ids, ["footprint", "group", "zero", "unknown"]);
    }

    #[test]
    fn selection_preserves_exact_group_across_refresh_and_exited_detail_is_explicit() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        let mut next = snapshot();
        let mut second = next.groups[0].clone();
        second.id = "new-instance".into();
        second.memory_bytes = Metric::ok(200 * 1024 * 1024, "test");
        next.groups.push(second);
        app.received(Ok(next));
        assert_eq!(app.selected_group.as_deref(), Some("group"));
        assert_eq!(app.table.selected(), Some(1));
        app.handle(key(KeyCode::Enter));
        let mut next = snapshot();
        next.groups.clear();
        app.received(Ok(next));
        let (text, _) = screen(&mut app, 100, 24);
        assert!(text.contains("旧实例不会映射到替代进程"));
    }

    #[test]
    fn pending_uses_shared_policy_and_home_has_no_periodic_collection() {
        let mut app = App::new(false);
        app.received(Ok(snapshot()));
        app.sampled = Some(Instant::now() - Duration::from_secs(5));
        assert!(!app.should_refresh(Duration::from_secs(2)));
        app.handle(key(KeyCode::Char('2')));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 90, 24);
        assert!(text.contains("当前预演没有待确认对象"));
        assert!(app.should_refresh(Duration::from_secs(2)));
        app.loading = true;
        assert!(!app.should_refresh(Duration::from_secs(2)));
    }

    #[test]
    fn failed_initial_read_and_failed_refresh_are_distinct() {
        let mut app = App::new(false);
        app.received(Err("permission denied".into()));
        let (text, _) = screen(&mut app, 100, 24);
        assert!(text.contains("读取失败"));
        assert!(!text.contains("已用 0"));
        app.received(Ok(snapshot()));
        app.received(Err("unavailable".into()));
        let (text, _) = screen(&mut app, 100, 24);
        assert!(text.contains("旧数据"));
    }

    #[test]
    fn excessive_page_down_stays_on_details_and_home_returns_to_first_row() {
        let mut app = App::new(true);
        let mut sample = snapshot();
        sample.processes[0].executable_path =
            Some(format!("/private/{}", "很长的项目路径".repeat(30)));
        app.received(Ok(sample));
        app.handle(key(KeyCode::Enter));
        let (_, mut terminal) = screen(&mut app, 60, 16);
        for _ in 0..50 {
            app.handle(key(KeyCode::PageDown));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
        assert!(app.detail_scroll > 0, "narrow viewport needs scrolling");
        let bottom = buffer_text(terminal.backend().buffer());
        assert!(
            bottom.contains("尚未验证"),
            "bottom retains actual detail rows"
        );
        let last_scroll = app.detail_scroll;
        app.handle(key(KeyCode::PageDown));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            app.detail_scroll, last_scroll,
            "cannot scroll into blank rows"
        );
        app.handle(key(KeyCode::Home));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.detail_scroll, 0);
        assert!(buffer_text(terminal.backend().buffer()).contains("中文应用"));
    }

    #[test]
    fn detail_scroll_reclamps_after_resize_and_refresh() {
        let mut app = App::new(true);
        let mut sample = snapshot();
        sample.processes[0].executable_path =
            Some(format!("/private/{}", "很长的项目路径".repeat(30)));
        app.received(Ok(sample.clone()));
        app.handle(key(KeyCode::Enter));
        let (_, mut terminal) = screen(&mut app, 60, 16);
        for _ in 0..50 {
            app.handle(key(KeyCode::PageDown));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.detail_scroll > 0);
        terminal.backend_mut().resize(120, 40);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            app.detail_scroll, 0,
            "large viewport fits the shorter wrapped content"
        );
        assert!(buffer_text(terminal.backend().buffer()).contains("PID 123"));

        terminal.backend_mut().resize(60, 16);
        for _ in 0..50 {
            app.handle(key(KeyCode::PageDown));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.detail_scroll > 0);
        sample.groups.clear();
        app.received(Ok(sample));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            app.detail_scroll, 0,
            "missing object notice must stay visible"
        );
        assert!(buffer_text(terminal.backend().buffer()).contains("此对象已不在最近采样中"));
    }

    #[test]
    fn control_c_and_q_exit_even_with_notice_open() {
        let mut app = App::new(false);
        app.notice = Some("test".into());
        assert!(matches!(
            app.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputResult::Quit(true)
        ));
        assert!(matches!(
            app.handle(key(KeyCode::Char('Q'))),
            InputResult::Quit(false)
        ));
    }

    #[test]
    fn terminal_theme_inherits_background_and_normal_text_even_in_popups() {
        let store = TestStore::new();
        let mut app = store.app();
        app.received(Ok(scoped_snapshot("Fixture")));
        for (width, height) in [(100, 32), (60, 20), (30, 10)] {
            let (_, terminal) = screen(&mut app, width, height);
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.bg == Color::Reset)
            );
        }
        app.page = Page::Detail(app.snapshot.as_ref().unwrap().groups[0].id.clone());
        app.handle(key(KeyCode::Char('a')));
        let (text, terminal) = screen(&mut app, 100, 32);
        assert!(text.contains("确认保存"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol() == "安" && cell.fg == Color::Reset)
        );
    }

    #[test]
    fn brand_adapts_to_desktop_and_compact_windows_without_hiding_navigation() {
        let mut app = App::new(false);
        let (large, _) = screen(&mut app, 100, 32);
        assert!(large.contains("█▄▄▄"));
        assert!(large.contains("`-(_  __)-'"));
        assert!(large.contains("bree · 规则与预演"));
        assert!(large.contains("4. 处理记录"));
        assert!(large.contains("S 设置"));
        let (compact, _) = screen(&mut app, 60, 20);
        assert!(compact.contains("( -.- ) bree"));
        assert!(!compact.contains("█▄▄▄"));
        assert!(compact.contains("4. 处理记录"));
        assert!(compact.contains("Q 退出"));
    }

    #[test]
    fn details_require_exact_scope_confirmation_and_cancel_does_not_save() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        let id = sample.groups[0].id.clone();
        app.received(Ok(sample));
        app.page = Page::Detail(id);
        app.handle(key(KeyCode::Char('a')));
        let (text, _) = screen(&mut app, 110, 32);
        assert!(text.contains("com.example.fixture"));
        assert!(text.contains("/Applications/Fixture.app/Contents/MacOS/Fixture"));
        assert!(text.contains("当前不会发送退出请求"));
        app.handle(key(KeyCode::Esc));
        assert!(store.0.load().unwrap().rules.is_empty());
        assert!(
            !store.0.root().exists(),
            "cancelled rule does not create storage"
        );
        app.handle(key(KeyCode::Char('p')));
        app.handle(key(KeyCode::Enter));
        let state = store.0.load().unwrap();
        assert_eq!(state.rules.len(), 1);
        assert_eq!(state.rules[0].action, RuleAction::Protect);
        assert_eq!(app.plan.as_ref().unwrap().protected_count, 1);
    }

    #[test]
    fn settings_view_and_revoke_only_selected_rule_preserve_newer_changes() {
        let store = TestStore::new();
        let first = scoped_snapshot("First");
        let second = scoped_snapshot("Second");
        let third = scoped_snapshot("Third");
        let scope = |sample: &Snapshot| scope_for_group(sample, &sample.groups[0].id).unwrap();
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope: scope(&first),
            })
            .unwrap();
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Allow,
                scope: scope(&second),
            })
            .unwrap();
        let mut app = store.app();
        app.handle(key(KeyCode::Char('s')));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 110, 30);
        assert!(text.contains("/Applications/First.app/Contents/MacOS/First"));
        app.handle(key(KeyCode::Char('d')));
        app.handle(key(KeyCode::Esc));
        assert_eq!(store.0.load().unwrap().rules.len(), 2);
        app.handle(key(KeyCode::Char('d')));
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope: scope(&third),
            })
            .unwrap();
        app.handle(key(KeyCode::Enter));
        let state = store.0.load().unwrap();
        assert_eq!(state.rules.len(), 2);
        assert_eq!(state.revision, 4);
        assert!(
            !state
                .rules
                .iter()
                .any(|rule| rule.scope.bundle_path.contains("First.app"))
        );
        assert!(
            state
                .rules
                .iter()
                .any(|rule| rule.scope.bundle_path.contains("Second.app"))
        );
        assert!(
            state
                .rules
                .iter()
                .any(|rule| rule.scope.bundle_path.contains("Third.app"))
        );
    }

    #[test]
    fn corrupt_state_during_confirmation_is_preserved_and_disables_save() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        let scope = scope_for_group(&sample, &sample.groups[0].id).unwrap();
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope,
            })
            .unwrap();
        app.reload_policy();
        let id = sample.groups[0].id.clone();
        app.received(Ok(sample));
        app.page = Page::Detail(id);
        app.handle(key(KeyCode::Char('a')));
        let path = store.0.root().join("state.json");
        std::fs::write(&path, b"broken configuration").unwrap();
        app.handle(key(KeyCode::Enter));
        assert!(app.policy_error.is_some());
        assert_eq!(
            app.policy.rules.len(),
            1,
            "preserve the last read-only state"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"broken configuration");
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Char('s')));
        let (text, _) = screen(&mut app, 100, 30);
        assert!(text.contains("当前只读"));
        app.handle(key(KeyCode::Char('d')));
        assert!(app.confirmation.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken configuration");
    }

    #[test]
    fn preview_has_zero_automatic_candidates_and_stays_idle_with_frozen_revision() {
        let store = TestStore::new();
        let mut app = store.app();
        app.received(Ok(scoped_snapshot("Fixture")));
        app.open_preview();
        let plan = app.preview_plan.as_ref().unwrap();
        assert!(plan.read_only);
        assert_eq!(plan.automatic_count, 0);
        assert_eq!(plan.pending_count, 1);
        assert_eq!(plan.rule_revision, 0);
        let (text, _) = screen(&mut app, 100, 30);
        assert!(text.contains("自动候选 0"));
        assert!(text.contains("采样 UTC"));
        assert!(text.contains("尚无用户允许规则"));
        assert!(!app.should_refresh(Duration::ZERO));
        assert!(
            std::fs::read_to_string(store.0.root().join("journal.jsonl"))
                .unwrap()
                .contains("dry_run_prepared")
        );
        let snapshot = scoped_snapshot("Fixture");
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope: scope_for_group(&snapshot, &snapshot.groups[0].id).unwrap(),
            })
            .unwrap();
        app.reload_policy();
        let (text, _) = screen(&mut app, 100, 30);
        assert!(text.contains("当前预演为旧修订"));
        assert_eq!(app.preview_plan.as_ref().unwrap().rule_revision, 0);
    }

    #[test]
    fn development_filter_uses_verified_labels_without_changing_group_category() {
        let mut app = App::new(true);
        let mut sample = snapshot();
        sample.processes[0].uid = Some(unsafe { libc::geteuid() });
        sample.processes[0].executable_path = Some("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex".into());
        sample.groups[0].category = Category::Unknown;
        app.received(Ok(sample));
        app.set_filter(Filter::Ai);
        assert_eq!(app.groups().len(), 1);
        assert_eq!(app.groups()[0].category, Category::Unknown);
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 110, 45);
        assert!(text.contains("开发标签 Codex CLI"));
        assert!(text.contains("不代表任务完成"));
    }

    #[test]
    fn save_failure_preserves_in_memory_and_persisted_rules() {
        use std::os::unix::fs::PermissionsExt;
        let store = TestStore::new();
        let sample = scoped_snapshot("Fixture");
        let scope = scope_for_group(&sample, &sample.groups[0].id).unwrap();
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope,
            })
            .unwrap();
        let mut app = store.app();
        app.page = Page::Detail(sample.groups[0].id.clone());
        app.received(Ok(sample));
        app.handle(key(KeyCode::Char('a')));
        let original = std::fs::read(store.0.root().join("state.json")).unwrap();
        std::fs::set_permissions(store.0.root(), std::fs::Permissions::from_mode(0o500)).unwrap();
        app.handle(key(KeyCode::Enter));
        assert!(app.policy_error.is_some());
        assert_eq!(app.policy.rules.len(), 1);
        assert_eq!(app.policy.revision, 1);
        assert_eq!(
            std::fs::read(store.0.root().join("state.json")).unwrap(),
            original
        );
        assert!(app.notice.as_deref().unwrap().contains("保存失败"));
    }

    #[test]
    fn busy_preview_remains_an_explicit_failure_after_dismissing_notice() {
        let store = TestStore::new();
        let _execution = store.0.execution_lock().unwrap();
        let mut app = store.app();
        app.received(Ok(scoped_snapshot("Fixture")));
        app.open_preview();
        assert!(app.preview_error.is_some());
        assert_eq!(app.preview_plan.as_ref().unwrap().automatic_count, 0);
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 110, 32);
        assert!(text.contains("预演未完成"));
        assert!(text.contains("未记录成功预演"));
        assert!(!store.0.root().join("journal.jsonl").exists());
    }

    fn prepared_record_count(store: &Store) -> usize {
        std::fs::read_to_string(store.root().join("journal.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                    == "dry_run_prepared"
            })
            .count()
    }

    #[test]
    fn late_resource_sample_does_not_replace_frozen_preview_or_append_a_record() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        let scope = scope_for_group(&sample, &sample.groups[0].id).unwrap();
        let sampled_label = since_sample(&sample);
        app.received(Ok(sample.clone()));
        app.page = Page::Resources;
        app.loading = true; // The resources page already has a request in flight.
        app.handle(key(KeyCode::Esc));
        app.handle(key(KeyCode::Char('p')));
        let frozen = app.preview_plan.as_ref().unwrap().plan_id.clone();
        assert_eq!(prepared_record_count(&store.0), 1);
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope,
            })
            .unwrap();
        let mut late = sample;
        late.sampled_at_unix_ms = 200_000;
        late.collected_in_ms = 88;
        app.received(Ok(late));
        assert_eq!(app.preview_plan.as_ref().unwrap().plan_id, frozen);
        assert_eq!(app.preview_plan.as_ref().unwrap().rule_revision, 0);
        assert_eq!(store.0.load().unwrap().revision, 1);
        assert_eq!(app.preview_sample.as_deref(), Some(sampled_label.as_str()));
        assert_eq!(prepared_record_count(&store.0), 1);
        let (text, _) = screen(&mut app, 110, 32);
        assert!(text.contains(&sampled_label));
        assert!(
            !text.contains("00:03:20"),
            "the frozen plan must keep its own sample label"
        );
    }

    #[test]
    fn explicit_preview_refresh_retires_old_plan_and_prepares_exactly_once() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        app.received(Ok(sample.clone()));
        app.open_preview();
        let old_id = app.preview_plan.as_ref().unwrap().plan_id.clone();
        store
            .0
            .change(RuleChange::Add {
                action: RuleAction::Protect,
                scope: scope_for_group(&sample, &sample.groups[0].id).unwrap(),
            })
            .unwrap();
        assert!(matches!(
            app.handle(key(KeyCode::Char('r'))),
            InputResult::Refresh
        ));
        assert!(app.preview_plan.is_none());
        assert!(app.preview_sample.is_none());
        app.loading = true; // The run loop now issues the explicitly requested refresh.
        let mut fresh = sample.clone();
        fresh.sampled_at_unix_ms = 300_000;
        app.received(Ok(fresh));
        let plan = app.preview_plan.as_ref().unwrap();
        assert_ne!(plan.plan_id, old_id);
        assert_eq!(plan.rule_revision, 1);
        assert_eq!(plan.protected_count, 1);
        assert_eq!(prepared_record_count(&store.0), 2);
        app.received(Ok(sample));
        assert_eq!(prepared_record_count(&store.0), 2);
        assert!(app.preview_sample.as_deref().unwrap().contains("00:05:00"));
    }

    #[test]
    fn preview_refresh_during_collection_waits_then_requests_a_new_sample() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        app.received(Ok(sample.clone()));
        app.open_preview();
        app.loading = true;
        app.handle(key(KeyCode::Char('r')));
        assert!(app.preview_refresh_queued);
        assert!(!app.take_queued_preview_refresh());
        app.received(Ok(sample.clone())); // This was already in flight before R.
        assert!(app.preview_plan.is_none());
        assert_eq!(prepared_record_count(&store.0), 1);
        assert!(app.take_queued_preview_refresh());
        assert!(
            !app.take_queued_preview_refresh(),
            "the fresh request is taken once"
        );
        app.loading = true;
        let mut fresh = sample;
        fresh.sampled_at_unix_ms = 400_000;
        app.received(Ok(fresh));
        assert!(app.preview_sample.as_deref().unwrap().contains("00:06:40"));
        assert_eq!(prepared_record_count(&store.0), 2);
    }

    #[test]
    fn first_sample_initializes_preview_once_and_failed_refresh_has_no_new_plan() {
        let store = TestStore::new();
        let mut app = store.app();
        app.open_preview();
        assert!(app.preview_plan.is_none());
        assert_eq!(prepared_record_count(&store.0), 0);
        app.received(Ok(scoped_snapshot("Fixture")));
        assert!(app.preview_plan.is_some());
        assert_eq!(prepared_record_count(&store.0), 1);
        app.received(Ok(scoped_snapshot("Fixture")));
        assert_eq!(prepared_record_count(&store.0), 1);
        app.handle(key(KeyCode::Char('r')));
        app.received(Err("sample unavailable".into()));
        assert!(app.preview_plan.is_none());
        assert!(app.preview_sample.is_none());
        assert!(app.preview_error.as_deref().unwrap().contains("采样失败"));
        assert_eq!(prepared_record_count(&store.0), 1);
        let (text, _) = screen(&mut app, 110, 32);
        assert!(text.contains("没有新计划，也未记录成功预演"));
        assert!(text.contains("sample unavailable"));
    }

    fn cleanup_result() -> CleanupResult {
        use crate::cleanup::{Outcome, TargetResult};
        let sample = snapshot();
        let targets = [
            Outcome::Exited,
            Outcome::StillRunning,
            Outcome::RequestRefused,
            Outcome::Skipped,
            Outcome::Cancelled,
            Outcome::Unknown,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| TargetResult {
            group_id: format!("group-{index}"),
            name: format!("测试应用 {index}"),
            identity: Some(sample.processes[0].identity.clone()),
            outcome,
            reason: format!("测试依据 {index}"),
            request_sent: index < 3,
            observed_ms: if outcome == Outcome::StillRunning {
                15_000
            } else {
                20
            },
        })
        .collect();
        CleanupResult {
            schema_version: 1,
            run_id: "run:test".into(),
            plan_id: "plan:test".into(),
            rule_revision: 2,
            started_at_unix_ms: 1_000,
            finished_at_unix_ms: 20_000,
            cancelled: false,
            targets,
            resource_before: sample.system.clone(),
            resource_after: Some(sample.system),
            resource_observation: "测试观察：内存没有下降，压力没有改善。".into(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn unavailable_cleanup_and_zero_candidates_do_not_start_a_session_or_write_records() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        let id = sample.groups[0].id.clone();
        app.received(Ok(sample));
        assert_eq!(app.plan.as_ref().unwrap().automatic_count, 0);
        app.handle(key(KeyCode::Char('1')));
        app.handle(key(KeyCode::Enter));
        assert!(app.cleanup.is_none());
        assert_eq!(app.page, Page::Home);
        assert!(app.notice.as_deref().unwrap().contains("没有发送"));
        app.handle(key(KeyCode::Enter));
        app.page = Page::Detail(id);
        app.handle(key(KeyCode::Char('e')));
        assert!(app.confirmation.is_none());
        assert!(app.cleanup.is_none());
        assert!(!store.0.root().exists());
    }

    #[test]
    fn manual_exit_confirmation_freezes_scope_and_instance_without_saving_a_rule() {
        let store = TestStore::new();
        let mut app = store.app();
        let sample = scoped_snapshot("Fixture");
        let scope = scope_for_group(&sample, &sample.groups[0].id).unwrap();
        let identity = sample.processes[0].identity.clone();
        app.confirmation = Some(RuleConfirmation::Exit {
            group_id: sample.groups[0].id.clone(),
            name: "测试应用".into(),
            scope,
            identity,
            snapshot: Box::new(sample),
        });
        let mut replacement = scoped_snapshot("Fixture");
        replacement.processes[0].identity.start_microseconds = Some(99);
        replacement.processes[0].id = replacement.processes[0].identity.object_id();
        replacement.groups[0].id = format!("app:{}", replacement.processes[0].id);
        replacement.groups[0].process_ids = vec![replacement.processes[0].id.clone()];
        app.received(Ok(replacement));
        match app.confirmation.as_ref().unwrap() {
            RuleConfirmation::Exit {
                identity, snapshot, ..
            } => {
                assert_eq!(identity.start_microseconds, Some(5));
                assert_eq!(snapshot.processes[0].identity.start_microseconds, Some(5));
            }
            _ => panic!("single-instance consent must keep its frozen snapshot"),
        }
        let (text, terminal) = screen(&mut app, 110, 36);
        assert!(text.contains("单次正常退出确认"));
        assert!(text.contains("PID 123"));
        assert!(text.contains("10 秒 + 5 微秒"));
        assert!(text.contains("不新增允许规则"));
        assert!(text.contains("替代实例会跳过"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
        app.handle(key(KeyCode::Esc));
        assert!(app.confirmation.is_none());
        assert!(app.cleanup.is_none());
        assert!(store.0.load().unwrap().rules.is_empty());
        assert!(!store.0.root().exists());
    }

    #[test]
    fn cleanup_results_separate_all_exit_outcomes_from_unchanged_or_missing_resources() {
        use crate::cleanup::Outcome;
        let mut app = App::new(false);
        app.page = Page::Results;
        app.cleanup_result = Some(cleanup_result());
        let (text, terminal) = screen(&mut app, 150, 65);
        assert!(text.contains("应用退出结果"));
        assert!(text.contains("系统资源观察"));
        for outcome in [
            Outcome::Exited,
            Outcome::StillRunning,
            Outcome::RequestRefused,
            Outcome::Skipped,
            Outcome::Cancelled,
            Outcome::Unknown,
        ] {
            assert!(text.contains(outcome.text()));
        }
        assert!(text.contains("内存没有下降，压力没有改善"));
        assert!(text.contains("没有改善也是合法结果"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
        app.cleanup_result.as_mut().unwrap().resource_after = None;
        let (text, _) = screen(&mut app, 150, 65);
        assert!(text.contains("未取得资源采样，不能判断改善情况"));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Home);
        assert!(app.cleanup.is_none());
    }

    #[test]
    fn history_views_finished_and_unfinished_runs_without_replaying_or_creating_files() {
        let store = TestStore::new();
        let mut app = store.app();
        app.handle(key(KeyCode::Char('4')));
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.page, Page::History);
        assert!(app.history.is_empty());
        assert!(!store.0.root().exists());
        store
            .0
            .append_record(
                "cleanup_finished",
                serde_json::to_value(cleanup_result()).unwrap(),
            )
            .unwrap();
        store
            .0
            .append_record(
                "cleanup_started",
                serde_json::json!({"run_id":"run:unfinished", "target_count": 1}),
            )
            .unwrap();
        let original = std::fs::read(store.0.root().join("journal.jsonl")).unwrap();
        app.handle(key(KeyCode::Char('r')));
        assert_eq!(app.history.len(), 2);
        let (text, _) = screen(&mut app, 120, 30);
        assert!(text.contains("已完成"));
        assert!(text.contains("未完成 / 无最终结果"));
        let unfinished = app
            .history
            .iter()
            .position(|item| item.result.is_none())
            .unwrap();
        app.history_selection.select(Some(unfinished));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 120, 32);
        assert!(text.contains("没有最终结果"));
        assert!(text.contains("Bree 不会重放"));
        app.handle(key(KeyCode::Esc));
        let finished = app
            .history
            .iter()
            .position(|item| item.result.is_some())
            .unwrap();
        app.history_selection.select(Some(finished));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 150, 65);
        assert!(text.contains("应用退出结果"));
        assert!(text.contains("系统资源观察"));
        assert!(app.cleanup.is_none());
        assert_eq!(
            std::fs::read(store.0.root().join("journal.jsonl")).unwrap(),
            original
        );
    }

    #[test]
    fn clean_command_waiting_intent_is_cancelled_by_navigation_or_failed_sampling() {
        for code in [KeyCode::Esc, KeyCode::Char('p'), KeyCode::Char('s')] {
            let store = TestStore::new();
            let mut app = store.app();
            app.start_cleanup_after_sample = true;
            app.handle(key(code));
            assert!(!app.start_cleanup_after_sample);
            assert!(app.cleanup.is_none());
            assert!(!store.0.root().exists());
        }
        let store = TestStore::new();
        let mut app = store.app();
        app.start_cleanup_after_sample = true;
        app.received(Err("test sampling failure".into()));
        assert!(!app.start_cleanup_after_sample);
        assert!(app.cleanup.is_none());
        assert!(app.notice.as_deref().unwrap().contains("清理未开始"));
        assert!(!store.0.root().exists());
    }

    #[test]
    fn running_cleanup_blocks_navigation_and_shows_per_instance_request_state() {
        let mut app = App::new(false);
        app.cleanup = Some(Session::test_running());
        app.page = Page::Executing;
        let (text, terminal) = screen(&mut app, 120, 36);
        assert!(text.contains("请求已发送"));
        assert!(text.contains("尚未核验"));
        assert!(text.contains("拒绝或仍在运行不会触发强制结束"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
        for code in [
            KeyCode::Char('s'),
            KeyCode::Char('r'),
            KeyCode::Char('p'),
            KeyCode::Enter,
        ] {
            assert!(matches!(app.handle(key(code)), InputResult::Continue));
            assert_eq!(app.page, Page::Executing);
            assert!(app.cleanup.is_some());
        }
        app.cancel_cleanup();
        assert_eq!(app.page, Page::Results);
    }

    #[test]
    fn control_c_escape_and_q_cancel_the_session_before_leaving_execution() {
        for key_event in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Esc),
            key(KeyCode::Char('q')),
        ] {
            let mut app = App::new(false);
            app.cleanup = Some(Session::test_running());
            app.page = Page::Executing;
            let exit = app.handle(key_event);
            if key_event.code == KeyCode::Char('q') {
                assert!(matches!(exit, InputResult::Quit(false)));
            } else {
                assert!(matches!(exit, InputResult::Continue));
            }
            assert!(app.cleanup.is_none());
            assert_eq!(app.page, Page::Results);
            let result = app.cleanup_result.as_ref().unwrap();
            assert!(result.cancelled);
            assert_eq!(result.targets.len(), 1);
            assert!(result.targets[0].request_sent);
            assert_eq!(result.targets[0].outcome, crate::cleanup::Outcome::Unknown);
            let (text, _) = screen(&mut app, 120, 40);
            assert!(text.contains("本次已取消"));
            assert!(text.contains("尚未核验"));
        }
    }

    #[test]
    fn history_read_failure_keeps_visible_cache_and_does_not_replay_actions() {
        let store = TestStore::new();
        store
            .0
            .append_record(
                "cleanup_finished",
                serde_json::to_value(cleanup_result()).unwrap(),
            )
            .unwrap();
        let mut app = store.app();
        app.open_history();
        assert_eq!(app.history.len(), 1);
        std::fs::write(store.0.root().join("journal.jsonl"), b"corrupt journal").unwrap();
        app.handle(key(KeyCode::Char('r')));
        assert!(app.history_error.is_some());
        assert_eq!(app.history.len(), 1);
        let (text, _) = screen(&mut app, 120, 32);
        assert!(text.contains("记录读取失败"));
        assert!(text.contains("缓存记录可能过期"));
        assert!(app.cleanup.is_none());
        assert_eq!(
            std::fs::read(store.0.root().join("journal.jsonl")).unwrap(),
            b"corrupt journal"
        );
    }

    #[test]
    fn clean_waiting_intent_is_retired_by_menu_navigation_even_when_returning_home() {
        for choice in ['2', '3', '4'] {
            for return_home in [false, true] {
                let store = TestStore::new();
                let mut app = store.app();
                app.loading = true;
                app.start_cleanup_after_sample = true;
                app.handle(key(KeyCode::Char(choice)));
                app.handle(key(KeyCode::Enter));
                assert!(
                    !app.start_cleanup_after_sample,
                    "menu {choice} must retire waiting clean"
                );
                let expected_page = if return_home {
                    app.handle(key(KeyCode::Esc));
                    Page::Home
                } else if choice == '4' {
                    Page::History
                } else {
                    Page::Resources
                };
                assert_eq!(app.page, expected_page);
                app.received(Ok(scoped_snapshot("Fixture")));
                assert_eq!(
                    app.page, expected_page,
                    "late result cannot force execution"
                );
                assert!(app.cleanup.is_none());
                assert!(app.cleanup_result.is_none());
                assert!(
                    app.notice.is_none(),
                    "late result must not attempt Session::start"
                );
                assert!(
                    !store.0.root().exists(),
                    "inspection navigation creates no execution record"
                );
                let records = std::fs::read_to_string(store.0.root().join("journal.jsonl"))
                    .unwrap_or_default();
                assert!(!records.contains("cleanup_started"));
            }
        }
    }
}
