//! Read-only memory inspection UI without persistent state.
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
use crate::brand::{self, ColorDepth};
use crate::collect::{Collector, pump_platform_events};
use crate::model::{Category, OccupancyGroup, ProcessInfo, Snapshot, safe_text};
use crate::output::{metric_bytes, missing_text, pressure_text};
use crate::query::{
    GroupSort as Sort, MAX_SEARCH_CHARS, compare_groups, group_matches, validate_search,
};

const ACCENT: Color = Color::Rgb(190, 86, 24);
const FOREGROUND: Color = Color::Reset;
const MUTED: Color = Color::Reset;
const BACKGROUND: Color = Color::Reset;
const MIN_WIDTH: u16 = 48;
const MIN_HEIGHT: u16 = 16;
const MASCOT_MIN_WIDTH: u16 = 60;
const MASCOT_MIN_HEIGHT: u16 = 26;
const WORDMARK_MIN_HEIGHT: u16 = 22;
const MASCOT_GAP: u16 = 3;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Application,
    Ai,
    System,
}

impl Filter {
    const ALL: [Self; 4] = [Self::All, Self::Application, Self::Ai, Self::System];

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
            Self::System => category == Category::System,
            // Development labels decide this in group_in_filter.
            Self::Ai => false,
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
    color_depth: ColorDepth,
    selected_group: Option<String>,
    detail_scroll: u16,
    notice: Option<String>,
    sampled: Option<Instant>,
    refresh_interval: Duration,
}

impl App {
    fn new(watch: bool) -> Self {
        let mut menu = ListState::default();
        menu.select(Some(0));
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
            // Tests must not inherit the developer's terminal; color tests set it explicitly.
            color_depth: if cfg!(test) {
                ColorDepth::None
            } else {
                ColorDepth::from_environment()
            },
            selected_group: None,
            detail_scroll: 0,
            notice: None,
            sampled: None,
            refresh_interval: Duration::from_secs(2),
        }
    }

    fn group_has_development_label(&self, group: &OccupancyGroup) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.processes.iter().any(|process| {
                group.process_ids.contains(&process.id) && label_process(process).is_some()
            })
        })
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
                self.snapshot = Some(snapshot);
                self.error = None;
                self.reconcile_selection();
            }
            Err(error) => {
                self.error = Some(safe_text(&error));
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
            return InputResult::Quit(true);
        }
        // Editor text must be handled before application shortcuts such as Q or R.
        if self.search_editor.is_some() {
            self.edit_search(key);
            return InputResult::Continue;
        }
        if matches!(key.code, KeyCode::Char('q' | 'Q')) {
            return InputResult::Quit(false);
        }
        if self.notice.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.notice = None;
            }
            return InputResult::Continue;
        }
        match key.code {
            KeyCode::Char('r' | 'R') => return InputResult::Refresh,
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
                Page::Home => {}
            },
            _ => match &self.page {
                Page::Home => match key.code {
                    KeyCode::Enter => {
                        self.set_filter(Filter::All);
                        self.page = Page::Resources;
                    }
                    KeyCode::Char('1') => self.menu.select(Some(0)),
                    _ => {}
                },
                Page::Resources => match key.code {
                    KeyCode::Char('/') => self.begin_search(),
                    KeyCode::Up | KeyCode::Char('k') => self.move_row(false),
                    KeyCode::Down | KeyCode::Char('j') => self.move_row(true),
                    KeyCode::Tab | KeyCode::Right => {
                        self.set_filter(Filter::ALL[(self.filter.index() + 1) % Filter::ALL.len()])
                    }
                    KeyCode::BackTab | KeyCode::Left => self.set_filter(
                        Filter::ALL
                            [(self.filter.index() + Filter::ALL.len() - 1) % Filter::ALL.len()],
                    ),
                    KeyCode::Char(value @ '1'..='4') => {
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
                Page::Detail(_) => match key.code {
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
            },
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
    run_internal(watch, interval)
}

fn run_internal(watch: bool, interval: Duration) -> Result<bool, String> {
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
    app.refresh_interval = interval.max(Duration::from_millis(100));
    // Show the skeleton before native initialization or process enumeration.
    let mut worker = None;
    let refresh_interval = app.refresh_interval;
    // Include the first frame in the explicit teardown path: the terminal can
    // disappear while that skeleton is still flushing.
    let first_frame = terminal
        .draw(|frame| render(frame, &mut app))
        .map(|_| ())
        .map_err(|error| format!("Terminal draw failed: {error}"));
    let result =
        first_frame.and_then(|_| run_loop(&mut terminal, &mut app, &mut worker, refresh_interval));
    // Restore the terminal before waiting for an in-flight, read-only snapshot to finish.
    // ratatui-core 0.1.2's Terminal::drop uses eprintln! if cursor restoration
    // fails. When both output and stderr are the disconnected PTY, that diagnostic
    // panics. Contain only dependency teardown so the guard and worker still drop,
    // and retain the original I/O error when one was already observed.
    let teardown = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(terminal)));
    drop(guard);
    drop(worker);
    if teardown.is_err() && result.is_ok() {
        Err("Terminal cursor restoration failed after output disconnected.".into())
    } else {
        result
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    worker: &mut Option<Worker>,
    interval: Duration,
) -> Result<bool, String> {
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
        if redraw {
            terminal
                .draw(|frame| render(frame, app))
                .map_err(|error| format!("Terminal draw failed: {error}"))?;
            redraw = false;
        }
        // Blocking poll avoids a rendering/animation loop while still receiving worker data.
        let timeout = if app.loading {
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
        Category::System => "System",
        Category::Unknown => "Unknown",
    }
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
            Paragraph::new(
                "bree · Memory\nResize to at least 48 columns × 16 rows.\nQ or Ctrl+C Quit",
            )
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
    let mascot = home_mascot_rect(app, area);
    let expanded = area.width >= MASCOT_MIN_WIDTH - 4 && area.height >= WORDMARK_MIN_HEIGHT - 2;
    let header_height = if mascot.is_some() {
        brand::HEIGHT + 1
    } else if expanded {
        4
    } else {
        2
    };
    if let Some(mascot) = mascot {
        brand::render(frame, mascot, app.color_depth);
        let wordmark = Rect::new(
            mascot.right() + MASCOT_GAP,
            area.y + (brand::HEIGHT - 3) / 2,
            area.width - brand::WIDTH - MASCOT_GAP,
            3,
        );
        frame.render_widget(Paragraph::new(brand_header(true)), wordmark);
    } else {
        frame.render_widget(
            Paragraph::new(brand_header(expanded)),
            Rect::new(area.x, area.y, area.width, header_height),
        );
    }
    render_home_content(
        frame,
        app,
        Rect::new(
            area.x,
            area.y + header_height,
            area.width,
            area.height - header_height,
        ),
    );
}

fn home_mascot_rect(app: &App, area: Rect) -> Option<Rect> {
    (app.color_depth != ColorDepth::None
        && area.width >= MASCOT_MIN_WIDTH - 4
        && area.height >= MASCOT_MIN_HEIGHT - 2)
        .then(|| Rect::new(area.x, area.y, brand::WIDTH, brand::HEIGHT))
}

fn render_home_content(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let summary_height = 5;
    let footer_gap = u16::from(area.height > summary_height + 1 + 2);
    let chunks = Layout::vertical([
        Constraint::Length(summary_height),
        Constraint::Length(1),
        Constraint::Length(footer_gap),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(summary(app, area.width < 76)).wrap(Wrap { trim: false }),
        chunks[0],
    );
    let entries = ["1. Memory        Grouped by application"];
    let list = List::new(entries.map(ListItem::new))
        .highlight_symbol(selection_marker())
        .highlight_style(selection_style());
    frame.render_stateful_widget(list, chunks[1], &mut app.menu);
    frame.render_widget(
        Paragraph::new("1 Select  Enter Open  R Refresh\nQ Quit")
            .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[3],
    );
}

fn display_memory(metric: &crate::model::Metric<u64>, compact: bool) -> String {
    if compact && (metric.status != crate::model::Validity::Ok || metric.value.is_none()) {
        // Short forms fit the 9-column compact memory cell.
        match metric.status {
            crate::model::Validity::Denied => "Denied".into(),
            crate::model::Validity::Unsupported => "N/A".into(),
            status => missing_text(status).into(),
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
            "Analyzing…\nReading real memory data.".into()
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
    let tabs = Tabs::new(["All", "Apps", "AI/Dev", "System"])
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
                let values = if compact {
                    vec![
                        safe_text(&group.name),
                        display_memory(&group.memory_bytes, true),
                        group.process_ids.len().to_string(),
                    ]
                } else {
                    vec![
                        safe_text(&group.name),
                        metric_bytes(&group.memory_bytes),
                        group.process_ids.len().to_string(),
                        category_name(group.category).into(),
                        safe_text(&group.metric_kind),
                    ]
                };
                Row::new(values)
            })
            .collect();
        let (header, widths) = if compact {
            (
                vec!["Name", "Memory", "Procs"],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(9),
                    Constraint::Length(5),
                ],
            )
        } else {
            (
                vec!["Name", "Memory", "Procs", "Type", "Metric"],
                vec![
                    Constraint::Min(16),
                    Constraint::Length(12),
                    Constraint::Length(5),
                    Constraint::Length(11),
                    Constraint::Length(12),
                ],
            )
        };
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
        let explanation =
            "Group sum; different metrics stay separate. This is not recoverable memory.";
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
    let footer =
        format!("↑↓ Select  Enter Details  Tab Filter\nO Sort R Refresh {esc} Q Quit\n{note}",);
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[5],
    );
}

fn process_detail(process: &ProcessInfo) -> Vec<Line<'static>> {
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
        Paragraph::new("↑↓/PgDn Scroll\nR Refresh  Esc Back  Q Quit")
            .style(Style::default().fg(MUTED).add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ActivationPolicy, Application, Attribution, Coverage, Metric, Pressure, ProcessIdentity,
        SystemMemory, Validity,
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
            schema_version: crate::model::SCHEMA_VERSION,
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

    fn scoped_snapshot(installation: &str) -> Snapshot {
        let mut sample = snapshot();
        let process = &mut sample.processes[0];
        process.uid = Some(unsafe { libc::geteuid() });
        process.executable_path = Some(format!(
            "/Applications/{installation}.app/Contents/MacOS/{installation}"
        ));
        process.attribution = Attribution {
            application: Some(Application {
                bundle_id: Some("com.example.fixture".into()),
                bundle_path: format!("/Applications/{installation}.app"),
                name: "中文应用".into(),
                leader_pid: process.identity.pid,
                frontmost: false,
                activation_policy: ActivationPolicy::Regular,
            }),
            method: "appkit_main_application".into(),
            confidence: "high".into(),
            explanation: "Current AppKit main instance".into(),
        };
        sample.groups[0].id = format!("app:{}", process.id);
        sample
    }

    #[test]
    fn initial_focus_opens_memory_and_returns_home() {
        let mut app = App::new(false);
        assert_eq!(app.menu.selected(), Some(0));
        let (text, _) = screen(&mut app, 48, 16);
        assert!(text.contains("Analyzing"));
        assert!(text.contains("> 1. Memory"));
        app.handle(key(KeyCode::Char('1')));
        assert!(matches!(
            app.handle(key(KeyCode::Enter)),
            InputResult::Continue
        ));
        assert_eq!(app.page, Page::Resources);
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Home);
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
        assert!(text.contains("Verified process"));
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
    fn home_stays_idle_and_resources_refresh_periodically() {
        let mut app = App::new(false);
        app.received(Ok(snapshot()));
        app.sampled = Some(Instant::now() - Duration::from_secs(5));
        assert!(!app.should_refresh(Duration::from_secs(2)));
        app.handle(key(KeyCode::Char('1')));
        app.handle(key(KeyCode::Enter));
        let (text, _) = screen(&mut app, 90, 24);
        assert!(text.contains("bree Memory"));
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
            bottom.contains("CPU 1.0%"),
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
        let mut app = App::new(false);
        app.color_depth = ColorDepth::None;
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
        app.notice = Some("Inspection notice".into());
        let (text, terminal) = screen(&mut app, 100, 32);
        assert!(text.contains("Inspection notice"));
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
        app.color_depth = ColorDepth::None;
        app.received(Ok(snapshot()));
        for (width, height) in [
            (100, 28),
            (140, 40),
            (60, 26),
            (60, 25),
            (60, 22),
            (59, 22),
            (60, 21),
            (48, 16),
        ] {
            let (text, terminal) = screen(&mut app, width, height);
            assert_eq!(
                text.contains("█▄▄▄"),
                width >= MASCOT_MIN_WIDTH && height >= WORDMARK_MIN_HEIGHT,
                "{width}x{height}: {text}"
            );
            for label in ["> 1. Memory", "Q Quit"] {
                assert!(
                    text.contains(label),
                    "{width}x{height} lacks {label}: {text}"
                );
            }
            assert!(!text.contains("-.-"));
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

    fn assert_mascot_canvas(buffer: &ratatui::buffer::Buffer, mascot: Option<Rect>) {
        for y in buffer.area.top()..buffer.area.bottom() {
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let cell = &buffer[(x, y)];
                let inside = mascot.is_some_and(|rect| {
                    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
                });
                assert!(!cell.modifier.contains(Modifier::REVERSED));
                if cell.bg != Color::Reset {
                    assert!(inside, "background outside sprite at {x},{y}: {cell:?}");
                    assert_eq!(cell.symbol(), "▀");
                }
                if !inside {
                    assert_eq!(cell.bg, Color::Reset);
                    assert!(
                        matches!(cell.fg, Color::Reset) || cell.fg == ACCENT,
                        "foreground outside sprite at {x},{y}: {cell:?}"
                    );
                } else if cell.symbol() == " " {
                    assert_eq!(cell.fg, Color::Reset);
                    assert_eq!(cell.bg, Color::Reset);
                }
                // TestBackend retains an old value under a wide glyph's continuation;
                // a terminal paints that cell with the leading glyph's current style.
                x += Line::from(cell.symbol()).width().max(1) as u16;
            }
        }
    }

    #[test]
    fn compact_mascot_home_keeps_real_data_menu_and_failures_visible_in_both_color_modes() {
        for depth in [ColorDepth::Rgb, ColorDepth::Indexed256] {
            for state in 0..4 {
                let mut app = App::new(false);
                app.color_depth = depth;
                match state {
                    1 => app.received(Err(
                        "A long initial read failure; no data could be read".into()
                    )),
                    2 => app.received(Ok(snapshot())),
                    3 => {
                        let mut sample = snapshot();
                        sample.system.used_bytes =
                            Metric::unavailable(Validity::Denied, "fixture", "permission denied");
                        sample.system.total_bytes =
                            Metric::unavailable(Validity::Denied, "fixture", "permission denied");
                        app.received(Ok(sample));
                        app.received(Err(
                            "Long refresh failure with details that must not hide the menu".into(),
                        ));
                    }
                    _ => {}
                }
                let (text, terminal) = screen(&mut app, 60, 26);
                let buffer = terminal.backend().buffer();
                let sprite = Rect::new(2, 1, brand::WIDTH, brand::HEIGHT);
                assert_mascot_canvas(buffer, Some(sprite));
                assert_eq!(buffer[(25, 4)].symbol(), "█");
                assert_eq!(buffer[(25, 4)].fg, ACCENT);
                assert!(buffer.content.iter().any(|cell| cell.bg != Color::Reset));
                assert_eq!(app.menu.selected(), Some(0));
                for label in ["> 1. Memory", "R Refresh", "Q Quit"] {
                    assert!(
                        text.contains(label),
                        "depth {depth:?}, state {state}: missing {label}\n{text}"
                    );
                }
                match state {
                    0 => assert!(text.contains("Analyzing")),
                    1 => assert!(
                        text.contains("Read failed") && text.contains("Unknown memory is not zero")
                    ),
                    2 => assert!(text.contains("Memory pressure") && text.contains("8.00 GiB")),
                    3 => assert!(text.contains("Denied") && text.contains("Previous data")),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn mascot_thresholds_and_resize_clear_all_old_image_cells_without_changing_selection() {
        let mut app = App::new(false);
        app.color_depth = ColorDepth::Rgb;
        app.received(Ok(snapshot()));
        for (width, height, full) in [
            (60, 26, true),
            (140, 40, true),
            (59, 26, false),
            (60, 25, false),
            (60, 22, false),
            (60, 21, false),
            (80, 24, false),
            (80, 26, true),
            (48, 16, false),
        ] {
            let (text, terminal) = screen(&mut app, width, height);
            assert_eq!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .any(|cell| cell.bg != Color::Reset),
                full,
                "{width}x{height}"
            );
            assert_mascot_canvas(
                terminal.backend().buffer(),
                full.then(|| Rect::new(2, 1, brand::WIDTH, brand::HEIGHT)),
            );
            assert!(text.contains("> 1. Memory") && text.contains("Q Quit"));
        }
        let (_, mut terminal) = screen(&mut app, 60, 26);
        terminal.backend_mut().resize(60, 25);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_mascot_canvas(terminal.backend().buffer(), None);
        assert!(buffer_text(terminal.backend().buffer()).contains("█▄▄▄"));
        terminal.backend_mut().resize(48, 16);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_mascot_canvas(terminal.backend().buffer(), None);
        assert!(buffer_text(terminal.backend().buffer()).contains("> 1. Memory"));
        terminal.backend_mut().resize(60, 26);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_mascot_canvas(
            terminal.backend().buffer(),
            Some(Rect::new(2, 1, brand::WIDTH, brand::HEIGHT)),
        );
        app.page = Page::Resources;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_mascot_canvas(terminal.backend().buffer(), None);
        app.page = Page::Home;
        app.color_depth = ColorDepth::None;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_mascot_canvas(terminal.backend().buffer(), None);
        assert!(buffer_text(terminal.backend().buffer()).contains("█▄▄▄"));
    }

    #[test]
    fn home_body_stays_below_the_brand_and_left_aligned_with_nearby_navigation() {
        for depth in [ColorDepth::Rgb, ColorDepth::Indexed256, ColorDepth::None] {
            for (width, height) in [(60, 26), (60, 25), (60, 22), (140, 40), (48, 16)] {
                let mut app = App::new(false);
                app.color_depth = depth;
                app.received(Ok(snapshot()));
                let (text, _) = screen(&mut app, width, height);
                let lines: Vec<_> = text.lines().collect();
                let summary = lines
                    .iter()
                    .position(|line| line.starts_with("  Memory pressure:"))
                    .expect("memory overview stays on the left baseline");
                let menu = lines
                    .iter()
                    .position(|line| line.starts_with("  > 1. Memory"))
                    .expect("selection marker stays on the left baseline");
                let footer = lines
                    .iter()
                    .position(|line| line.starts_with("  1 Select"))
                    .expect("navigation stays on the left baseline");
                assert!(summary < menu && menu < footer);
                assert!(footer - menu <= 2, "navigation follows the menu: {text}");
                if depth != ColorDepth::None && width >= 60 && height >= 26 {
                    assert!(summary > brand::HEIGHT as usize);
                }
            }
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
        let (text, _) = screen(&mut app, 48, 16);
        for label in [
            "Denied",
            "Unknown",
            "Previous data",
            "> 1. Memory",
            "R Refresh",
            "Q Quit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        assert!(!text.contains("Used 0"));
        app.page = Page::Resources;
        let (text, _) = screen(&mut app, 48, 16);
        for label in ["Previous data", "R Refresh", "Esc Home", "Q Quit"] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
    }

    #[test]
    fn compact_secondary_pages_keep_their_navigation_and_inherited_colors() {
        let mut app = App::new(false);
        app.received(Ok(scoped_snapshot("Fixture")));
        let detail = app.snapshot.as_ref().unwrap().groups[0].id.clone();
        for page in [Page::Resources, Page::Detail(detail)] {
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
        app.notice = Some("Inspection notice".into());
        let (text, _) = screen(&mut app, 48, 16);
        assert!(text.contains("Inspection notice"));
        assert!(text.contains("Enter / Esc Back"));
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
    fn compact_memory_cells_keep_missing_causes_distinct() {
        let cell =
            |status| display_memory(&Metric::<u64>::unavailable(status, "test", "missing"), true);
        let cells = [
            (Validity::Denied, "Denied"),
            (Validity::Unsupported, "N/A"),
            (Validity::Exited, "Exited"),
            (Validity::Stale, "Stale"),
            (Validity::Unknown, "Unknown"),
        ];
        for (status, expected) in cells {
            assert_eq!(cell(status), expected);
            assert!(expected.chars().count() <= 9);
        }
        assert_eq!(
            display_memory(
                &Metric::unavailable(Validity::Exited, "test", "gone"),
                false
            ),
            "— / Exited"
        );
    }
}
