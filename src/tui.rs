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
use crate::query::{
    GroupSort as Sort, MAX_SEARCH_CHARS, compare_groups, group_matches, validate_search,
};
use crate::storage::Store;

const ACCENT: Color = Color::Rgb(190, 86, 24);
const FOREGROUND: Color = Color::Reset;
const MUTED: Color = Color::Reset;
const BACKGROUND: Color = Color::Reset;
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
        enable_raw_mode().map_err(|error| format!("Cannot enter terminal input mode: {error}"))?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(format!("Cannot initialize the terminal: {error}"));
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

struct SearchEditor {
    draft: String,
    original_selection: Option<String>,
    error: Option<String>,
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
    query: String,
    search_editor: Option<SearchEditor>,
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
            query: String::new(),
            search_editor: None,
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
                "Rules unavailable; read-only mode. See Settings for details. Damaged configuration will be preserved.".into(),
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
                self.notice = Some(format!(
                    "Cannot set an application rule for this object: {}",
                    safe_text(&error)
                ))
            }
        }
    }

    fn begin_remove(&mut self) {
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some(
                "Rules unavailable. Repair the original configuration, then press R to reload."
                    .into(),
            );
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
                Some("Normal quit is disabled; read-only mode. An Allow rule does not enable this capability.".into());
            return;
        }
        self.reload_policy();
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice = Some(
                "Rules cannot be read; normal quit is disabled. See Settings for details.".into(),
            );
            return;
        }
        if let Some(entry) = self.entry(id)
            && entry.disposition == Disposition::Protected
        {
            self.notice = Some(format!(
                "This object is protected and cannot be asked to quit: {}",
                entry
                    .reasons
                    .iter()
                    .map(|reason| safe_text(reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let scope = match scope_for_group(snapshot, id) {
            Ok(scope) => scope,
            Err(error) => {
                self.notice = Some(format!(
                    "This object does not support normal quit: {}",
                    safe_text(&error)
                ));
                return;
            }
        };
        let Some(entry) = self.entry(id) else {
            return;
        };
        let Some(identity) = &entry.target_identity else {
            self.notice = Some(
                "The exact main application instance is missing; no quit request can be sent."
                    .into(),
            );
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
                "Normal quit is disabled; no automatic targets and no requests sent. P opens a read-only preview."
                    .into(),
            );
            return;
        }
        if self.policy_error.is_some() || self.store.is_none() {
            self.notice =
                Some("Rules cannot be read; cleanup is disabled. S opens Settings.".into());
            return;
        }
        if self
            .plan
            .as_ref()
            .is_none_or(|plan| plan.automatic_count == 0)
        {
            self.notice = Some("No automatic targets; no requests sent. Add an Allow rule in Details, or press P to see the reasons.".into());
            return;
        }
        if let Some(snapshot) = self.snapshot.clone() {
            self.start_cleanup(snapshot, CleanupMode::Automatic);
        }
    }

    fn start_cleanup(&mut self, snapshot: Snapshot, mode: CleanupMode) {
        let Some(store) = self.store.clone() else {
            self.notice = Some(
                "Cleanup not started: history storage unavailable; no quit requests sent.".into(),
            );
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
                    "Cleanup not started: {}; no other instance was included.",
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
            self.history_error = Some("History storage unavailable".into());
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
            self.notice = Some(
                "Cannot read rule state; save cancelled, read-only mode. See Settings for details."
                    .into(),
            );
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
                self.notice = Some("Rule saved. This changes future classification only; no quit request is sent. Remove the rule in Settings.".into());
            }
            Err(error) => {
                self.policy_error = Some(safe_text(&error));
                self.evaluate_policy();
                self.notice = Some(format!(
                    "Rule save failed; read-only mode: {}",
                    safe_text(&error)
                ));
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
            self.preview_error = Some("Rule storage unavailable".into());
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
                    "Preview incomplete: {}. No quit requests are sent.",
                    safe_text(&error)
                ));
            }
        }
    }

    fn groups(&self) -> Vec<&OccupancyGroup> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut groups: Vec<_> = snapshot
            .groups
            .iter()
            .filter(|group| self.group_in_filter(group))
            .filter(|group| group_matches(snapshot, group, &self.query))
            .collect();
        groups.sort_by(|left, right| compare_groups(left, right, self.sort));
        groups
    }

    fn group_in_filter(&self, group: &OccupancyGroup) -> bool {
        match self.filter {
            Filter::Pending => self
                .entry(&group.id)
                .is_some_and(|entry| entry.disposition == Disposition::Pending),
            Filter::Ai => self.group_has_development_label(group),
            _ => self.filter.accepts(group.category),
        }
    }

    fn begin_search(&mut self) {
        self.search_editor = Some(SearchEditor {
            draft: self.query.clone(),
            original_selection: self.selected_group.clone(),
            error: None,
        });
    }

    fn edit_search(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            if let Some(editor) = self.search_editor.take() {
                self.selected_group = editor.original_selection;
                self.reconcile_selection();
            }
            return;
        }
        let Some(editor) = &mut self.search_editor else {
            return;
        };
        match key.code {
            KeyCode::Enter => match validate_search(&editor.draft) {
                Ok(query) => {
                    self.query = query;
                    self.search_editor = None;
                    self.reconcile_selection();
                }
                Err(error) => editor.error = Some(error),
            },
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.draft.clear();
                editor.error = None;
            }
            KeyCode::Backspace => {
                editor.draft.pop();
                editor.error = None;
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut next = editor.draft.clone();
                next.push(value);
                match validate_search(&next) {
                    Ok(_) => {
                        editor.draft = next;
                        editor.error = None;
                    }
                    Err(error) => editor.error = Some(error),
                }
            }
            _ => {}
        }
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
                        "Cleanup not started: sampling failed: {}; no quit requests sent.",
                        safe_text(&error)
                    ));
                }
                if self.page == Page::Preview
                    && self.preview_plan.is_none()
                    && !self.preview_refresh_queued
                {
                    self.preview_error = Some(format!("Sampling failed: {}", safe_text(&error)));
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
        // Editor text must be handled before application shortcuts such as Q, R or S.
        if self.search_editor.is_some() {
            self.edit_search(key);
            return InputResult::Continue;
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
                self.notice = Some("Cleanup preparation cancelled; no quit requests sent.".into());
            }
            KeyCode::Esc => match self.page {
                Page::Detail(_) => {
                    self.page = Page::Resources;
                    self.detail_scroll = 0;
                }
                Page::Resources if !self.query.is_empty() => {
                    self.query.clear();
                    self.reconcile_selection();
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
                    KeyCode::Char('/') => self.begin_search(),
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
        .map_err(|_| "The sampling worker stopped; the terminal will be restored.".to_string())
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
        return Err(
            "The interactive UI requires a TTY. Use bree status --json or bree list --json.".into(),
        );
    }
    // With ctrlc's `termination` feature this also handles external SIGTERM/SIGHUP.
    // Raw-mode Ctrl+C remains a key event. The CLI must not install another handler for TUI.
    ctrlc::set_handler(|| exit_for_external_signal())
        .map_err(|error| format!("Cannot register terminal interrupt recovery: {error}"))?;
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|error| format!("Cannot create the terminal UI: {error}"))?;
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
                    return Err(
                        "The sampling worker stopped unexpectedly; terminal restored.".into(),
                    );
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
                .map_err(|error| format!("Terminal draw failed: {error}"))?;
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
        if event::poll(timeout).map_err(|error| format!("Cannot read terminal events: {error}"))? {
            match event::read().map_err(|error| format!("Cannot read terminal events: {error}"))? {
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
        Category::Application => "Apps",
        Category::AiDevelopment => "AI / Dev",
        Category::System => "System",
        Category::Unknown => "Unknown",
    }
}

fn action_name(action: RuleAction) -> &'static str {
    match action {
        RuleAction::Allow => "Allow",
        RuleAction::Protect => "Protect",
    }
}

fn disposition_name(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Automatic => "Automatic",
        Disposition::Pending => "Needs review",
        Disposition::Protected => "Protected",
    }
}

fn scope_lines(scope: &AppScope) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("Bundle ID: {}", safe_text(&scope.bundle_id))),
        Line::from(format!("Installation: {}", safe_text(&scope.bundle_path))),
        Line::from(format!(
            "Main executable: {}",
            safe_text(&scope.executable_path)
        )),
        Line::from("Rules match this exact installation; names, PIDs and wildcards are not used."),
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
    frame.render_widget(
        Paragraph::new(title("Settings · Allow / Protect")),
        chunks[0],
    );
    let status = match &app.policy_error {
        Some(error) => format!(
            "Cannot read rules; read-only: {}\nSave and remove are disabled. Original file preserved. R reloads.",
            safe_text(error)
        ),
        None => format!(
            "{} rules · rev {} · Protect overrides Allow\n{}",
            app.policy.rules.len(),
            app.policy.revision,
            if cleanup::enabled() {
                "Saving a rule does not quit the application."
            } else {
                "Normal quit is disabled."
            }
        ),
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    if app.policy.rules.is_empty() {
        frame.render_widget(Paragraph::new("No rules yet; zero automatic targets is valid.\nIn Details, A allows and P protects. Confirm the exact installation.\nSetting a rule does not quit the application.").wrap(Wrap { trim: false }), chunks[2]);
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
                    if rule.enabled { "" } else { " · disabled" }
                ))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(rules)
                .highlight_symbol(selection_marker())
                .highlight_style(selection_style()),
            chunks[2],
            &mut app.rules,
        );
    }
    frame.render_widget(Paragraph::new("↑↓ Select  Enter Scope  D Remove\nR Reload  Esc Home  Q Quit\nRemove affects this rule, not the old config.").style(Style::default().add_modifier(Modifier::DIM)), chunks[3]);
}

fn render_rule_detail(frame: &mut Frame<'_>, app: &mut App, id: &str, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("Rule scope")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(rule) = app.policy.rules.iter().find(|rule| rule.id == id) {
        lines.push(Line::styled(
            format!(
                "{} rule · {}",
                action_name(rule.action),
                safe_text(&rule.id)
            ),
            Style::default().fg(ACCENT),
        ));
        lines.extend(scope_lines(&rule.scope));
        lines.push(Line::from(format!(
            "Source {} · {} · created UTC ms {}",
            safe_text(&rule.source),
            if rule.enabled { "Enabled" } else { "Disabled" },
            rule.created_at_unix_ms
        )));
        lines.push(Line::from(
            "Saving or viewing rules sends no quit requests.",
        ));
    } else {
        lines.push(Line::from(
            "This rule was removed. Esc returns to Settings.",
        ));
    }
    if let Some(error) = &app.policy_error {
        lines.push(Line::from(format!(
            "Cannot read rules; old rules shown for reference: {}",
            safe_text(error)
        )));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("D Remove rule  ↑↓/PgDn Scroll\nR Reload  Esc Settings  Q Quit")
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
        Paragraph::new(title("Preview · No quit requests")),
        chunks[0],
    );
    let mut lines = Vec::new();
    if let Some(plan) = &app.preview_plan {
        lines.push(Line::styled(
            format!(
                "Automatic {} · Needs review {} · Protected {}",
                plan.automatic_count, plan.pending_count, plan.protected_count
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(if cleanup::enabled() {
            "No quit requests here. Execution rechecks the instance, protection and frontmost state."
        } else {
            "Normal quit is disabled. Allow rules affect classification only; zero automatic targets is valid."
        }));
        lines.push(Line::from(format!(
            "Plan {} · rule revision {} · created UTC ms {}",
            safe_text(&plan.plan_id),
            plan.rule_revision,
            plan.created_at_unix_ms
        )));
        if let Some(sample) = &app.preview_sample {
            lines.push(Line::from(sample.clone()));
        }
        lines.push(Line::from(
            "This preview freezes the sample and rules. R samples again; resource changes do not update this page.",
        ));
        if plan.rule_revision != app.policy.revision {
            lines.push(Line::styled(
                "Rules changed; preview uses an old revision. R rebuilds the preview.",
                Style::default().fg(ACCENT),
            ));
        }
        if app.error.is_some() {
            lines.push(Line::styled(
                "Refresh failed; the previous sample is shown.",
                Style::default().fg(ACCENT),
            ));
        }
        if let Some(error) = &app.policy_error {
            lines.push(Line::styled(
                format!(
                    "Rules unreadable; protected, read-only mode: {}",
                    safe_text(error)
                ),
                Style::default().fg(ACCENT),
            ));
        }
        if let Some(error) = &app.preview_error {
            lines.push(Line::styled(
                format!(
                    "Preview incomplete: {}; conservative reasons shown, no successful preview recorded.",
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
            format!(
                "Preview incomplete: {}. R samples again to retry.",
                safe_text(error)
            ),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::from(
            "No new plan or successful preview recorded; no quit requests are sent.",
        ));
    } else if app.loading || app.preview_refresh_queued {
        lines.push(Line::from(
            "Sampling for a new preview; no quit requests are sent.",
        ));
    } else {
        lines.push(Line::from(
            "No data for a preview. R samples again; no quit requests are sent.",
        ));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new(
            "↑↓/PgDn Scroll  R Rebuild preview\nS Settings  Esc Home  Q Quit · Read-only",
        )
        .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn result_lines(result: &CleanupResult) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!(
            "Run {} · rule revision {}",
            safe_text(&result.run_id),
            result.rule_revision
        )),
        Line::from(format!("Plan {}", safe_text(&result.plan_id))),
    ];
    if result.cancelled {
        lines.push(Line::styled(
            "Cancelled. Sent requests cannot be recalled; unverified outcomes remain explicit.",
            Style::default().fg(ACCENT),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Application quit outcomes",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if result.targets.is_empty() {
        lines.push(Line::from("No targets; no quit requests sent."));
    }
    for target in &result.targets {
        lines.push(Line::styled(
            format!("{} · {}", target.outcome.text(), safe_text(&target.name)),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!(
            "{} · observed {} ms",
            if target.request_sent {
                "Normal quit request sent"
            } else {
                "No request sent"
            },
            target.observed_ms
        )));
        if let Some(identity) = &target.identity {
            lines.push(Line::from(format!(
                "Exact instance {}",
                safe_text(&identity.object_id())
            )));
        }
        lines.push(Line::from(format!("Reason: {}", safe_text(&target.reason))));
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        "System resource observations",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(format!(
        "Before: used {} · pressure {} · compressed {} · swap {}",
        metric_bytes(&result.resource_before.used_bytes),
        pressure_text(&result.resource_before.pressure),
        metric_bytes(&result.resource_before.compressed_bytes),
        metric_bytes(&result.resource_before.swap_used_bytes)
    )));
    if let Some(after) = &result.resource_after {
        lines.push(Line::from(format!(
            "After: used {} · pressure {} · compressed {} · swap {}",
            metric_bytes(&after.used_bytes),
            pressure_text(&after.pressure),
            metric_bytes(&after.compressed_bytes),
            metric_bytes(&after.swap_used_bytes)
        )));
    } else {
        lines.push(Line::from(
            "After: no resource sample; improvement cannot be assessed.",
        ));
    }
    lines.push(Line::from(safe_text(&result.resource_observation)));
    lines.push(Line::from(
        "No improvement is a valid result. System changes are not a guarantee of memory freed by this run.",
    ));
    for error in &result.errors {
        lines.push(Line::styled(
            format!("History / observation error: {}", safe_text(error)),
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
    frame.render_widget(Paragraph::new(title("Normal quit · Progress")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(session) = &app.cleanup {
        lines.push(Line::styled(
            safe_text(session.progress()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!(
            "Frozen plan {} · rule revision {}",
            safe_text(&session.plan().plan_id),
            session.plan().rule_revision
        )));
        lines.push(Line::from(
            "Each request rechecks the exact instance, installation, protection and frontmost state.",
        ));
        lines.push(Line::from(
            "Observe for up to 15 s. Refusal or continued running never triggers force quit.",
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
                    "Request sent"
                } else {
                    "No request sent"
                },
                safe_text(&target.reason)
            )));
            lines.push(Line::from(""));
        }
    } else {
        lines.push(Line::from("Execution ended. Esc returns Home."));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("Ctrl+C / Esc Cancel; stop further requests\nQ Cancel and quit · Actions are not replayed")
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
    frame.render_widget(Paragraph::new(title("Results")), chunks[0]);
    let lines = app
        .cleanup_result
        .as_ref()
        .map(result_lines)
        .unwrap_or_else(|| {
            vec![Line::from(
                "No completed result. History can show unfinished runs.",
            )]
        });
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new("↑↓ / PgUp / PgDn Scroll\nEsc Home  S Settings  Q Quit")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn history_status(status: &str) -> String {
    match status {
        "finished" => "Finished".into(),
        "cancelled" => "Cancelled".into(),
        "unfinished" => "Unfinished".into(),
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
    frame.render_widget(Paragraph::new(title("History · Recent runs")), chunks[0]);
    let status = match &app.history_error {
        Some(error) => format!(
            "History read failed: {}\nCached records may be stale. R retries; actions are not replayed.",
            safe_text(error)
        ),
        None => format!(
            "{} runs · retained for 7 days, up to 10 MiB\nFinished and unfinished shown separately. Viewing never executes a run.",
            app.history.len()
        ),
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    if app.history.is_empty() {
        frame.render_widget(
            Paragraph::new("No cleanup runs yet. Rule and preview logs are not quit outcomes.")
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
                        "{} · {} · UTC ms {}",
                        history_status(&item.status),
                        if item.result.is_some() {
                            "Result recorded"
                        } else {
                            "Unfinished / no final result"
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
            List::new(items)
                .highlight_symbol(selection_marker())
                .highlight_style(selection_style()),
            chunks[2],
            &mut app.history_selection,
        );
    }
    frame.render_widget(
        Paragraph::new("↑↓ Select  Enter Results  R Reload\nEsc Home  Q Quit · No replay")
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
    frame.render_widget(Paragraph::new(title("Run details")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(item) = app.history.iter().find(|item| item.run_id == id) {
        lines.push(Line::styled(
            format!(
                "{} · UTC ms {}",
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
                "Run {} has no final result.",
                safe_text(&item.run_id)
            )));
            lines.push(Line::from(
                "The final outcome of previously sent requests is unknown. Bree will not replay this run.",
            ));
        }
    } else {
        lines.push(Line::from(
            "This run is no longer in recent history. Esc returns to the list.",
        ));
    }
    if let Some(error) = &app.history_error {
        lines.push(Line::styled(
            format!("Read failed; cached records shown: {}", safe_text(error)),
            Style::default().fg(ACCENT),
        ));
    }
    render_scrolled(frame, lines, chunks[1], &mut app.detail_scroll);
    frame.render_widget(
        Paragraph::new(
            "↑↓ / PgUp / PgDn Scroll  R Reload\nEsc History  Q Quit · View only, no replay",
        )
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
            lines.push(Line::from(format!("Target: {}", safe_text(name))));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "This persistent rule requires confirmation. No quit requests are sent. Protect takes priority.",
            ));
            format!(" New {} rule ", action_name(*action))
        }
        RuleConfirmation::Remove { id, action, scope } => {
            lines.push(Line::from(format!(
                "Remove {} rule {}",
                action_name(*action),
                safe_text(id)
            )));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Remove only this rule; the old configuration is not restored and no application is asked to quit.",
            ));
            " Remove selected rule ".into()
        }
        RuleConfirmation::Exit {
            name,
            scope,
            identity,
            ..
        } => {
            lines.push(Line::from(format!("Application: {}", safe_text(name))));
            lines.extend(scope_lines(scope));
            lines.push(Line::from(format!(
                "Exact instance: {}",
                safe_text(&identity.object_id())
            )));
            lines.push(Line::from(format!(
                "PID {} · started {} s + {} µs",
                identity.pid,
                identity
                    .start_seconds
                    .map_or_else(|| "unknown".into(), |value| value.to_string()),
                identity
                    .start_microseconds
                    .map_or_else(|| "unknown".into(), |value| value.to_string())
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Request normal quit once for this exact instance.",
            ));
            lines.push(Line::from("This one-time action adds no Allow rule."));
            lines.push(Line::from("Refusal or timeout never triggers force quit."));
            lines.push(Line::from(
                "Recheck before execution; a replacement instance will be skipped.",
            ));
            " Confirm normal quit ".into()
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
            "Enter Request quit  Esc Cancel\n↑↓ / PgDn Review installation and instance"
        } else {
            "Enter Save rule  Esc Cancel\n↑↓ / PgDn Review installation scope"
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
        "Sample UTC {:02}:{:02}:{:02} · {} ms",
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
    let brand = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    if !expanded {
        return vec![Line::styled("bree", brand), Line::from("")];
    }
    // A compact lowercase wordmark uses ordinary terminal cells, with no subtitle.
    [
        "█▄▄▄  ▄ ▄▄  ▄▄▄▄  ▄▄▄▄",
        "█  █  █▀   █▄▄▄█ █▄▄▄█",
        "█▄▄█  █     ▀▄▄▄  ▀▄▄▄",
        "",
    ]
    .into_iter()
    .map(|line| Line::styled(line, brand))
    .collect()
}

fn selection_marker() -> Line<'static> {
    Line::styled(
        "> ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

fn selection_style() -> Style {
    Style::default().fg(FOREGROUND).add_modifier(Modifier::BOLD)
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(FOREGROUND).bg(BACKGROUND)),
        area,
    );
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(if app.cleanup.is_some() { "bree · Quit observation\nResize to at least 48 columns × 16 rows.\nCtrl+C Cancel  Q Cancel and quit" } else { "bree · Memory\nResize to at least 48 columns × 16 rows.\nQ or Ctrl+C Quit" })
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
            Paragraph::new(format!("{}\n\nEnter / Esc Back", safe_text(notice)))
                .style(Style::default().fg(FOREGROUND).bg(BACKGROUND))
                .block(
                    Block::bordered()
                        .title(" Notice ")
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
        Constraint::Length(if expanded { 4 } else { 2 }),
        Constraint::Length(summary_height),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(brand_header(expanded)), chunks[0]);
    frame.render_widget(
        Paragraph::new(summary(app, area.width < 76)).wrap(Wrap { trim: false }),
        chunks[1],
    );
    let pending = app.plan.as_ref().map_or_else(
        || "Analyzing".into(),
        |plan| format!("{} · See reasons", plan.pending_count),
    );
    let automatic = app.plan.as_ref().map_or(0, |plan| plan.automatic_count);
    let entries = [
        if !cleanup::enabled() {
            "1. Clean         Normal quit disabled".into()
        } else if automatic == 0 {
            "1. Clean         No automatic targets".into()
        } else {
            format!("1. Clean         {automatic} allowed instances")
        },
        format!("2. Needs review  {pending}"),
        "3. Memory        Grouped by application".into(),
        "4. History       Completed / unfinished".into(),
    ];
    let list = List::new(entries.map(ListItem::new))
        .highlight_symbol(selection_marker())
        .highlight_style(selection_style());
    frame.render_stateful_widget(list, chunks[2], &mut app.menu);
    frame.render_widget(
        Paragraph::new("↑↓ / 1–4 Select  Enter Open  R Refresh\nP Preview  S Settings  Q Quit")
            .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[3],
    );
}

fn display_memory(metric: &crate::model::Metric<u64>, compact: bool) -> String {
    if compact && (metric.status != crate::model::Validity::Ok || metric.value.is_none()) {
        if metric.status == crate::model::Validity::Denied {
            "Denied".into()
        } else {
            "Unknown".into()
        }
    } else {
        metric_bytes(metric)
    }
}

fn summary(app: &App, compact: bool) -> Text<'static> {
    let Some(snapshot) = &app.snapshot else {
        let message = if let Some(error) = &app.error {
            format!(
                "Read failed; R retries.\nUnknown memory is not zero.\n{}",
                safe_text(error)
            )
        } else {
            format!(
                "Analyzing…\nReading real memory data; {}.",
                if cleanup::enabled() {
                    "cleanup requires explicitly allowed apps"
                } else {
                    "normal quit is disabled"
                }
            )
        };
        return Text::from(message);
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "Memory pressure: ",
                Style::default().fg(MUTED).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                pressure_text(&snapshot.system.pressure),
                Style::default().fg(ACCENT),
            ),
        ]),
        Line::from(format!(
            "Used {} / Total {}",
            display_memory(&snapshot.system.used_bytes, compact),
            display_memory(&snapshot.system.total_bytes, compact)
        )),
        Line::from(format!(
            "Compressed {} · Swap {}",
            display_memory(&snapshot.system.compressed_bytes, compact),
            display_memory(&snapshot.system.swap_used_bytes, compact)
        )),
        Line::from(format!(
            "{} · {}/{} read",
            since_sample(snapshot).replace("Sample ", ""),
            snapshot.coverage.readable_memory_processes,
            snapshot.coverage.enumerated_processes
        )),
    ];
    if let Some(error) = &app.error {
        lines.push(Line::styled(
            if compact {
                "Previous data · Refresh failed; R retry".into()
            } else {
                format!("Previous data · Refresh failed: {}", safe_text(error))
            },
            Style::default().fg(ACCENT),
        ));
    } else if app.loading {
        lines.push(Line::styled(
            "Refreshing · Previous sample shown",
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ));
    }
    if app.policy_error.is_some() {
        lines.push(Line::styled(
            "Rules unreadable · S Settings · Read-only",
            Style::default().fg(ACCENT),
        ));
    }
    Text::from(lines)
}

fn search_tail(value: &str, width: u16) -> String {
    if Line::from(value).width() <= width as usize {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut start = value.len();
    let mut used = 1;
    for (index, value) in value.char_indices().rev() {
        let next = Line::from(value.to_string()).width();
        if used + next > width as usize {
            break;
        }
        used += next;
        start = index;
    }
    format!("…{}", &value[start..])
}

fn render_resources(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let compact = area.width < 76;
    let wide = area.width >= 118;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(title("Memory")), chunks[0]);
    let status = if let Some(snapshot) = &app.snapshot {
        let sample = if app.error.is_some() {
            "Previous data · Refresh failed; R retry".into()
        } else if app.loading {
            "Refreshing · Previous sample shown".into()
        } else {
            since_sample(snapshot)
        };
        format!(
            "{} · Used {} / {}\n{}",
            pressure_text(&snapshot.system.pressure),
            display_memory(&snapshot.system.used_bytes, compact),
            display_memory(&snapshot.system.total_bytes, compact),
            sample
        )
    } else if let Some(error) = &app.error {
        format!("Read failed: {} · R Retry", safe_text(error))
    } else {
        "Analyzing… Reading real process memory".into()
    };
    frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), chunks[1]);
    let search = if let Some(editor) = &app.search_editor {
        Line::styled(
            format!(
                "Search: {}▏",
                search_tail(&editor.draft, area.width.saturating_sub(9))
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else if app.query.is_empty() {
        Line::styled(
            "/ Search names, Bundle ID or exact PID",
            Style::default().add_modifier(Modifier::DIM),
        )
    } else {
        Line::from(format!(
            "Search: {}",
            search_tail(&app.query, area.width.saturating_sub(8))
        ))
    };
    frame.render_widget(Paragraph::new(search), chunks[2]);
    let tabs = Tabs::new(["All", "Apps", "AI/Dev", "System", "Review"])
        .select(app.filter.index())
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider(" ")
        .padding("", "");
    frame.render_widget(tabs, chunks[3]);

    let groups = app.groups();
    let match_count = groups.len();
    let filter_count = app.snapshot.as_ref().map_or(0, |snapshot| {
        snapshot
            .groups
            .iter()
            .filter(|group| app.group_in_filter(group))
            .count()
    });
    if groups.is_empty() {
        let message = if app.snapshot.is_none() && app.loading {
            "Analyzing…"
        } else if app.snapshot.is_none() && app.error.is_some() {
            "Read failed. R retries."
        } else if !app.query.is_empty() {
            "No matches in this filter.\n/ Edit search · Esc Clear search"
        } else if app.filter == Filter::Pending {
            "No objects need review in this preview.\nSee All for protected objects and their reasons."
        } else if app.filter == Filter::Ai {
            "No reliable developer tool installation evidence matched this sample.\nUncovered objects remain in All; this does not prove there are no AI tasks."
        } else {
            "No objects in this filter."
        };
        frame.render_widget(
            Paragraph::new(message)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
            chunks[4],
        );
    } else {
        let rows: Vec<_> = groups
            .iter()
            .map(|group| {
                let mut values = if compact {
                    vec![
                        safe_text(&group.name),
                        display_memory(&group.memory_bytes, true),
                        group.process_ids.len().to_string(),
                        app.entry(&group.id)
                            .map_or("Unclassified", |entry| disposition_name(entry.disposition))
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
                            .map_or("Unclassified", |entry| disposition_name(entry.disposition))
                            .into(),
                    ]
                };
                if wide {
                    values.push(
                        app.entry(&group.id)
                            .and_then(|entry| entry.reasons.first())
                            .map(|reason| safe_text(reason))
                            .unwrap_or_else(|| "Awaiting classification".into()),
                    );
                }
                Row::new(values)
            })
            .collect();
        let (mut header, mut widths) = if compact {
            (
                vec!["Name", "Memory", "Procs", "State"],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(9),
                    Constraint::Length(5),
                    Constraint::Length(12),
                ],
            )
        } else {
            (
                vec!["Name", "Memory", "Procs", "Type", "Metric", "State"],
                vec![
                    Constraint::Min(16),
                    Constraint::Length(12),
                    Constraint::Length(5),
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(12),
                ],
            )
        };
        if wide {
            header.push("Reason");
            widths.push(Constraint::Min(26));
        }
        let table = Table::new(rows, widths)
            .header(Row::new(header).style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)))
            .row_highlight_style(selection_style())
            .highlight_symbol(selection_marker());
        frame.render_stateful_widget(table, chunks[4], &mut app.table);
    }
    if let Some(editor) = &app.search_editor {
        let note = editor.error.clone().unwrap_or_else(|| {
            format!(
                "{} / {MAX_SEARCH_CHARS} characters · Enter applies",
                editor.draft.chars().count()
            )
        });
        frame.render_widget(
            Paragraph::new(format!(
                "Enter Apply  Esc Cancel  Ctrl+U Clear\nBackspace Delete  Ctrl+C Quit\n{note}"
            ))
            .style(Style::default().add_modifier(Modifier::DIM)),
            chunks[5],
        );
        return;
    }
    let esc = if app.query.is_empty() {
        "Esc Home"
    } else {
        "Esc Clear"
    };
    let sort = if app.sort == Sort::Memory {
        "Memory"
    } else {
        "Name"
    };
    let note = if app.query.is_empty() {
        let explanation = app
            .selected_group
            .as_ref()
            .and_then(|id| app.entry(id))
            .and_then(|entry| entry.reasons.first())
            .map(|reason| format!("Reason: {}", safe_text(reason)))
            .unwrap_or_else(|| {
                "Group sum; different metrics stay separate. This is not recoverable memory.".into()
            });
        format!(
            "{sort} · {:.1}s refresh · {explanation}",
            app.refresh_interval.as_secs_f64()
        )
    } else {
        format!(
            "{match_count}/{filter_count} matching in filter · {sort} · {:.1}s",
            app.refresh_interval.as_secs_f64()
        )
    };
    let footer = format!(
        "↑↓ Select  Enter Details  Tab Filter\nO Sort R Refresh S Settings {esc} Q Quit\n{note}",
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[5],
    );
}

fn process_detail(process: &ProcessInfo) -> Vec<Line<'static>> {
    let protection = if process.protection_reasons.is_empty() {
        "No collection protection marker; use preview classification and execution rechecks".into()
    } else {
        process
            .protection_reasons
            .iter()
            .map(|reason| safe_text(reason))
            .collect::<Vec<_>>()
            .join("; ")
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
            "Memory {} · metric {}",
            metric_bytes(&process.memory_bytes),
            safe_text(&process.metric_kind)
        )),
        Line::from(format!(
            "Validity {:?} · source {}",
            process.memory_bytes.status,
            safe_text(&process.memory_bytes.source)
        )),
        Line::from(format!("Instance {}", safe_text(&process.id))),
        Line::from(format!(
            "Identity {:?} · started {} s + {} µs",
            process.identity.status,
            process
                .identity
                .start_seconds
                .map_or_else(|| "unknown".into(), |value| value.to_string()),
            process
                .identity
                .start_microseconds
                .map_or_else(|| "unknown".into(), |value| value.to_string())
        )),
        Line::from(format!(
            "Executable {}",
            process
                .executable_path
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "— / unavailable".into())
        )),
        Line::from(format!(
            "Attribution {} · confidence {}",
            safe_text(&attribution.method),
            safe_text(&attribution.confidence)
        )),
        Line::from(safe_text(&attribution.explanation)),
        Line::from(format!("Protection / limits {protection}")),
    ];
    if let Some(application) = application {
        lines.push(Line::from(format!(
            "Application {} · Bundle {} · frontmost {}",
            safe_text(&application.name),
            application
                .bundle_id
                .as_deref()
                .map(safe_text)
                .unwrap_or_else(|| "unknown".into()),
            if application.frontmost { "yes" } else { "no" }
        )));
        lines.push(Line::from(format!(
            "Application path {}",
            safe_text(&application.bundle_path)
        )));
    }
    if let Some(cpu) = process.cpu_one_core_percent.value {
        lines.push(Line::from(format!(
            "CPU {cpu:.1}% (one core) · source {}",
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
                .unwrap_or_else(|| "unavailable".into())
        )));
    }
    if let Some(reason) = &process.memory_bytes.reason {
        lines.push(Line::from(format!("Read note {}", safe_text(reason))));
    }
    if let Some(label) = label_process(process) {
        lines.push(Line::styled(
            format!("Developer label {}", label.label),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::from(format!(
            "Evidence {} · confidence {}",
            safe_text(&label.evidence),
            label.confidence
        )));
        lines.push(Line::from("The label explains attribution, not task completion or permission to stop automatically."));
    }
    lines.push(Line::from(
        if cleanup::enabled() && process.quit_supported {
            "Actions: normal quit for a main application after identity and protection checks. Ports: unverified."
        } else {
            "Actions: read-only; normal quit is disabled for this instance. Ports: unverified."
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
    frame.render_widget(Paragraph::new(title("Details")), chunks[0]);
    let mut lines = Vec::new();
    if let Some(snapshot) = &app.snapshot {
        if let Some(group) = snapshot.groups.iter().find(|group| group.id == id) {
            lines.push(Line::styled(
                safe_text(&group.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::from(format!(
                "Memory {} · {} processes · {}",
                metric_bytes(&group.memory_bytes),
                group.process_ids.len(),
                category_name(group.category)
            )));
            lines.push(Line::from(format!(
                "Metric {} · source {}",
                safe_text(&group.metric_kind),
                safe_text(&group.memory_bytes.source)
            )));
            lines.push(Line::from(safe_text(&group.explanation)));
            lines.push(Line::from(since_sample(snapshot)));
            if let Some(entry) = app.entry(id) {
                lines.push(Line::styled(
                    format!("Preview state {}", disposition_name(entry.disposition)),
                    Style::default().fg(ACCENT),
                ));
                for reason in &entry.reasons {
                    lines.push(Line::from(format!("· {}", safe_text(reason))));
                }
                if !entry.matched_rule_ids.is_empty() {
                    lines.push(Line::from(format!(
                        "Matched rules {}",
                        entry
                            .matched_rule_ids
                            .iter()
                            .map(|id| safe_text(id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
            lines.push(Line::from(
                "Group sum is not guaranteed freed memory. Refresh on demand.",
            ));
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
                "This object is absent from the latest sample. Its old instance will not map to a replacement; Esc returns to the list.",
            ));
        }
        if let Some(error) = &app.error {
            lines.push(Line::styled(
                format!("Refresh failed: {}; previous data shown", safe_text(error)),
                Style::default().fg(ACCENT),
            ));
        }
    } else {
        lines.push(Line::from("No data yet. R retries."));
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
            "A Allow  P Protect  E Quit  ↑↓/PgDn Scroll\nR Refresh  S Settings  Esc Back  Q Quit"
        } else {
            "A Allow  P Protect  E Disabled  ↑↓ Scroll\nR Refresh  S Settings  Esc Back  Q Quit"
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
                    method: "test".into(),
                    confidence: "high".into(),
                    explanation: "Verified process".into(),
                },
                protection_reasons: vec!["frontmost application".into()],
                quit_supported: false,
            }],
            groups: vec![OccupancyGroup {
                id: "group".into(),
                name: "中文应用".into(),
                category: Category::Application,
                memory_bytes: Metric::ok(100 * 1024 * 1024, "test_rss"),
                metric_kind: "rss".into(),
                process_ids: vec![id],
                explanation: "Test group".into(),
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
            explanation: "Current AppKit main instance".into(),
        };
        sample.groups[0].id = format!("app:{}", process.id);
        sample
    }

    #[test]
    fn initial_focus_and_read_only_preview_are_safe() {
        let mut app = App::new(false);
        assert_eq!(app.menu.selected(), Some(2));
        let (text, _) = screen(&mut app, 90, 24);
        assert!(text.contains("Analyzing"));
        assert!(text.contains("> 3. Memory"));
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
        assert!(text.contains("No quit requests"));
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
        assert!(text.contains("frontmost application"));
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
        assert!(text.contains("Resize"));
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
        assert!(text.contains("old instance will not map to a replacement"));
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
        assert!(text.contains("No objects need review"));
        assert!(app.should_refresh(Duration::from_secs(2)));
        app.loading = true;
        assert!(!app.should_refresh(Duration::from_secs(2)));
    }

    #[test]
    fn failed_initial_read_and_failed_refresh_are_distinct() {
        let mut app = App::new(false);
        app.received(Err("permission denied".into()));
        let (text, _) = screen(&mut app, 100, 24);
        assert!(text.contains("Read failed"));
        assert!(!text.contains("Used 0"));
        app.received(Ok(snapshot()));
        app.received(Err("unavailable".into()));
        let (text, _) = screen(&mut app, 100, 24);
        assert!(text.contains("Previous data"));
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
            bottom.contains("unverified"),
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
        assert!(
            buffer_text(terminal.backend().buffer())
                .contains("This object is absent from the latest sample")
        );
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
        assert!(text.contains("Save rule"));
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
                .any(|cell| cell.symbol() == "I" && cell.fg == Color::Reset)
        );
    }

    #[test]
    fn brand_adapts_to_desktop_and_compact_windows_without_hiding_navigation() {
        let mut app = App::new(false);
        app.received(Ok(snapshot()));
        for (width, height) in [(100, 28), (140, 40), (48, 16)] {
            let (text, terminal) = screen(&mut app, width, height);
            assert_eq!(
                text.contains("█▄▄▄"),
                width >= 80,
                "{width}x{height}: {text}"
            );
            for label in [
                "1. Clean",
                "2. Needs review",
                "> 3. Memory",
                "4. History",
                "S Settings",
                "Q Quit",
            ] {
                assert!(
                    text.contains(label),
                    "{width}x{height} lacks {label}: {text}"
                );
            }
            assert!(!text.contains("-.-"));
            assert!(!text.contains("Rules and preview"));
            assert!(!text.contains("Understand memory"));
            let buffer = terminal.backend().buffer();
            assert!(
                buffer
                    .content
                    .iter()
                    .all(|cell| cell.bg == Color::Reset
                        && !cell.modifier.contains(Modifier::REVERSED))
            );
            assert!(buffer.content.iter().any(|cell| cell.symbol() == ">"
                && cell.fg == ACCENT
                && cell.modifier.contains(Modifier::BOLD)));
        }
    }

    #[test]
    fn compact_home_keeps_navigation_visible_with_missing_metrics_and_refresh_failure() {
        let mut app = App::new(false);
        let mut sample = snapshot();
        sample.system.used_bytes =
            Metric::unavailable(Validity::Denied, "fixture", "permission denied");
        sample.system.total_bytes =
            Metric::unavailable(Validity::Denied, "fixture", "permission denied");
        sample.system.compressed_bytes =
            Metric::unavailable(Validity::Unsupported, "fixture", "unsupported");
        sample.system.swap_used_bytes =
            Metric::unavailable(Validity::Unknown, "fixture", "unknown");
        sample.system.pressure = Metric::unavailable(Validity::Unknown, "fixture", "unknown");
        app.received(Ok(sample));
        app.received(Err(
            "refresh failed with a long permission error and private path".into(),
        ));
        app.policy_error = Some("rules cannot be read".into());
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "Denied",
            "Unknown",
            "Previous data",
            "Rules unreadable",
            "1. Clean",
            "2. Needs review",
            "> 3. Memory",
            "4. History",
            "R Refresh",
            "S Settings",
            "Q Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        assert!(!text.contains("Used 0"));
        app.page = Page::Resources;
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "Previous data",
            "R Refresh",
            "S Settings",
            "Esc Home",
            "Q Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
    }

    #[test]
    fn compact_secondary_pages_keep_their_navigation_and_inherited_colors() {
        let store = TestStore::new();
        let mut app = store.app();
        app.received(Ok(scoped_snapshot("Fixture")));
        let detail = app.snapshot.as_ref().unwrap().groups[0].id.clone();
        for page in [
            Page::Settings,
            Page::Resources,
            Page::Detail(detail),
            Page::RuleDetail("missing".into()),
            Page::Preview,
            Page::Results,
            Page::History,
            Page::HistoryDetail("missing".into()),
        ] {
            app.page = page.clone();
            let (text, terminal) = screen(&mut app, 48, 16);
            for label in ["Esc", "Q Quit"] {
                assert!(text.contains(label), "{page:?} lacks {label}: {text}");
            }
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.bg == Color::Reset
                        && !cell.modifier.contains(Modifier::REVERSED))
            );
        }
        app.page = Page::Detail(app.snapshot.as_ref().unwrap().groups[0].id.clone());
        app.handle(key(KeyCode::Char('a')));
        let (text, _) = screen(&mut app, 48, 16);
        assert!(text.contains("Enter Save rule"));
        assert!(text.contains("Esc Cancel"));
    }

    fn input(app: &mut App, value: &str) {
        for value in value.chars() {
            assert!(matches!(
                app.handle(key(KeyCode::Char(value))),
                InputResult::Continue
            ));
        }
    }

    #[test]
    fn search_editor_treats_shortcuts_as_text_and_commits_only_on_enter() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        let selected = app.selected_group.clone();
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, "qrsa");
        assert_eq!(app.page, Page::Resources);
        assert_eq!(app.query, "");
        assert_eq!(app.search_editor.as_ref().unwrap().draft, "qrsa");
        assert_eq!(app.groups().len(), 1);
        assert_eq!(app.selected_group, selected);
        assert!(app.cleanup.is_none() && app.confirmation.is_none());
        app.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        input(&mut app, "  中文应用字  ");
        app.handle(key(KeyCode::Backspace));
        app.handle(key(KeyCode::Backspace));
        app.handle(key(KeyCode::Backspace));
        assert_eq!(app.search_editor.as_ref().unwrap().draft, "  中文应用");
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.query, "中文应用");
        assert!(app.search_editor.is_none());
        assert_eq!(app.groups().len(), 1);
        assert_eq!(app.selected_group, selected);
        app.handle(key(KeyCode::Char('/')));
        assert!(matches!(
            app.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputResult::Quit(true)
        ));
    }

    #[test]
    fn search_cancel_restores_the_original_exact_selection_after_refresh() {
        let mut app = App::new(true);
        let mut original = snapshot();
        let mut other = original.groups[0].clone();
        other.id = "other".into();
        other.name = "Other application".into();
        original.groups.push(other.clone());
        app.received(Ok(original.clone()));
        app.selected_group = Some("group".into());
        app.reconcile_selection();
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, "uncommitted");
        let mut temporarily_missing = original.clone();
        temporarily_missing.groups = vec![other];
        app.received(Ok(temporarily_missing.clone()));
        assert_eq!(app.selected_group.as_deref(), Some("other"));
        app.received(Ok(original));
        assert_eq!(app.selected_group.as_deref(), Some("other"));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.query, "");
        assert_eq!(app.selected_group.as_deref(), Some("group"));
        app.query = "Other".into();
        app.reconcile_selection();
        app.handle(key(KeyCode::Char('/')));
        app.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        input(&mut app, "new draft");
        app.received(Ok(temporarily_missing));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.query, "Other");
        assert_eq!(app.selected_group.as_deref(), Some("other"));
    }

    #[test]
    fn applied_search_composes_with_sort_filter_refresh_and_escape_clear() {
        let mut app = App::new(true);
        let mut original = snapshot();
        let mut other = original.groups[0].clone();
        other.id = "other".into();
        other.name = "Unmatched".into();
        other.category = Category::System;
        other.process_ids.clear();
        original.groups.push(other);
        app.received(Ok(original.clone()));
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, "中文");
        app.handle(key(KeyCode::Enter));
        let selected = app.selected_group.clone();
        app.handle(key(KeyCode::Char('o')));
        assert_eq!(app.sort, Sort::Name);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.filter, Filter::Application);
        assert_eq!(app.query, "中文");
        app.received(Ok(original));
        assert_eq!(app.query, "中文");
        assert_eq!(app.selected_group, selected);
        assert_eq!(app.groups().len(), 1);
        app.set_filter(Filter::System);
        assert!(app.groups().is_empty());
        assert!(app.selected_group.is_none());
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "Search: 中文",
            "No matches in this filter",
            "Esc Clear",
            "Q Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Resources);
        assert_eq!(app.query, "");
        assert_eq!(app.groups().len(), 1);
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Home);
    }

    #[test]
    fn narrow_search_editor_shows_the_unicode_tail_limit_and_validation_errors() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, &"中".repeat(MAX_SEARCH_CHARS));
        input(&mut app, "文");
        assert_eq!(
            app.search_editor.as_ref().unwrap().draft.chars().count(),
            MAX_SEARCH_CHARS
        );
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "Search: …",
            "▏",
            "128 characters",
            "Enter Apply",
            "Esc Cancel",
            "Ctrl+U Clear",
            "Ctrl+C Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        app.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        input(&mut app, "\u{202e}");
        assert_eq!(app.search_editor.as_ref().unwrap().draft, "");
        assert!(
            app.search_editor
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("bidirectional")
        );
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.query, "");
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "/ Search",
            "Enter Details",
            "Tab Filter",
            "O Sort",
            "R Refresh",
            "S Settings",
            "Q Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
    }

    #[test]
    fn search_details_return_and_clear_preserve_the_selected_exact_group() {
        let mut app = App::new(true);
        app.received(Ok(snapshot()));
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, "中文");
        app.handle(key(KeyCode::Enter));
        let selected = app.selected_group.clone();
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.page, Page::Detail("group".into()));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Resources);
        assert_eq!(app.query, "中文");
        assert_eq!(app.selected_group, selected);
        let (text, _) = screen(&mut app, 48, 16);
        assert!(text.contains("1/1 matching in filter"), "{text}");
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Resources);
        assert!(app.query.is_empty());
        assert_eq!(app.selected_group, selected);
    }

    #[test]
    fn cancelling_search_after_pid_reuse_never_restores_the_original_instance_id() {
        let mut app = App::new(true);
        let original = snapshot();
        let old_process_id = original.processes[0].id.clone();
        app.received(Ok(original.clone()));
        app.handle(key(KeyCode::Char('/')));
        input(&mut app, "uncommitted draft");
        let mut replacement = original;
        replacement.processes[0].identity.start_seconds = Some(11);
        replacement.processes[0].id = replacement.processes[0].identity.object_id();
        replacement.groups[0].id = format!("app:{}", replacement.processes[0].id);
        replacement.groups[0].process_ids = vec![replacement.processes[0].id.clone()];
        let new_group_id = replacement.groups[0].id.clone();
        app.received(Ok(replacement));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.query, "");
        assert_eq!(app.selected_group.as_ref(), Some(&new_group_id));
        assert_ne!(app.selected_group.as_deref(), Some("group"));
        assert_ne!(
            app.snapshot.as_ref().unwrap().processes[0].id,
            old_process_id
        );
        assert!(app.cleanup.is_none() && app.confirmation.is_none());
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
        assert!(text.contains("No quit requests are sent"));
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
        assert!(text.contains("read-only"));
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
        assert!(text.contains("Automatic 0"));
        assert!(text.contains("Sample UTC"));
        assert!(
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("no user allow rule")
        );
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
        assert!(text.contains("preview uses an old revision"));
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
        assert!(text.contains("Developer label Codex CLI"));
        assert!(text.contains("not task completion"));
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
        assert!(app.notice.as_deref().unwrap().contains("save failed"));
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
        assert!(text.contains("Preview incomplete"));
        assert!(text.contains("no successful preview recorded"));
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
        assert!(
            app.preview_error
                .as_deref()
                .unwrap()
                .contains("Sampling failed")
        );
        assert_eq!(prepared_record_count(&store.0), 1);
        let (text, _) = screen(&mut app, 110, 32);
        assert!(text.contains("No new plan or successful preview recorded"));
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
            reason: format!("Test reason {index}"),
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
            resource_observation:
                "Test observation: memory did not decrease and pressure did not improve.".into(),
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
        assert!(app.notice.as_deref().unwrap().contains("no requests sent"));
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
        assert!(text.contains("Confirm normal quit"));
        assert!(text.contains("PID 123"));
        assert!(text.contains("10 s + 5 µs"));
        assert!(text.contains("adds no Allow rule"));
        assert!(text.contains("replacement instance will be skipped"));
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
        assert!(text.contains("Application quit outcomes"));
        assert!(text.contains("System resource observations"));
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
        assert!(text.contains("memory did not decrease and pressure did not improve"));
        assert!(text.contains("No improvement is a valid result"));
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
        assert!(text.contains("no resource sample; improvement cannot be assessed"));
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
        assert!(text.contains("Finished"));
        assert!(text.contains("Unfinished / no final result"));
        let unfinished = app
            .history
            .iter()
            .position(|item| item.result.is_none())
            .unwrap();
        app.history_selection.select(Some(unfinished));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 120, 32);
        assert!(text.contains("has no final result"));
        assert!(text.contains("Bree will not replay"));
        app.handle(key(KeyCode::Esc));
        let finished = app
            .history
            .iter()
            .position(|item| item.result.is_some())
            .unwrap();
        app.history_selection.select(Some(finished));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 150, 65);
        assert!(text.contains("Application quit outcomes"));
        assert!(text.contains("System resource observations"));
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
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Cleanup not started")
        );
        assert!(!store.0.root().exists());
    }

    #[test]
    fn running_cleanup_blocks_navigation_and_shows_per_instance_request_state() {
        let mut app = App::new(false);
        app.cleanup = Some(Session::test_running());
        app.page = Page::Executing;
        let (text, terminal) = screen(&mut app, 120, 36);
        assert!(text.contains("Request sent"));
        assert!(text.contains("Not yet verified"));
        assert!(text.contains("Refusal or continued running never triggers force quit"));
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
            assert!(text.contains("Cancelled"));
            assert!(text.contains("Not yet verified"));
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
        assert!(text.contains("History read failed"));
        assert!(text.contains("Cached records may be stale"));
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
