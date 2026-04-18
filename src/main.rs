use std::{
    cmp::Ordering,
    io,
    process::Command,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};

const TOP_BIN: &str = "top";
const TOP_ARGS: [&str; 8] = [
    "-l",
    "2",
    "-s",
    "0",
    "-o",
    "power",
    "-stats",
    "pid,command,cpu,mem,power",
];
const REFRESH_EVERY: Duration = Duration::from_secs(2);
const REDRAW_EVERY: Duration = Duration::from_millis(120);
const HISTORY_LIMIT: usize = 240;

const COLOR_BG: Color = Color::Rgb(0x28, 0x2c, 0x34);
const COLOR_FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
const COLOR_ACCENT: Color = Color::Rgb(0x61, 0xaf, 0xef);
const COLOR_MUTED: Color = Color::Rgb(0x5c, 0x63, 0x70);
const COLOR_SELECTED_BG: Color = Color::Rgb(0x2c, 0x31, 0x3c);
const COLOR_GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
const COLOR_YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const COLOR_RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);

#[derive(Debug, Clone)]
struct ProcRow {
    pid: i32,
    process: String,
    process_lc: String,
    cpu: String,
    mem: String,
    power: String,
    power_num: f64,
    cpu_num: f64,
    mem_num: f64,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    rows: Vec<ProcRow>,
    total_power: f64,
    total_cpu: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Power,
    Cpu,
    Mem,
}

impl SortKey {
    fn as_str(self) -> &'static str {
        match self {
            SortKey::Power => "power",
            SortKey::Cpu => "cpu",
            SortKey::Mem => "mem",
        }
    }
}

#[derive(Debug)]
enum CollectorCommand {
    SetPaused(bool),
    Stop,
}

#[derive(Debug)]
enum CollectorEvent {
    Snapshot(Snapshot),
    Error(String),
}

struct App {
    snapshot: Snapshot,
    last_error: Option<String>,
    loading: bool,
    paused: bool,
    sort: SortKey,
    selected: usize,
    scroll: usize,
    filter_query: String,
    filter_input: Option<String>,
    power_history: Vec<f64>,
    cpu_history: Vec<f64>,
    visible_indices: Vec<usize>,
    visible_dirty: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            last_error: None,
            loading: true,
            paused: false,
            sort: SortKey::Power,
            selected: 0,
            scroll: 0,
            filter_query: String::new(),
            filter_input: None,
            power_history: Vec::new(),
            cpu_history: Vec::new(),
            visible_indices: Vec::new(),
            visible_dirty: true,
        };

        let visible_len = app.visible_len();
        app.normalize_selection(visible_len);
        app
    }

    fn active_filter(&self) -> &str {
        self.filter_input
            .as_deref()
            .unwrap_or(self.filter_query.as_str())
    }

    fn mark_visible_dirty(&mut self) {
        self.visible_dirty = true;
    }

    fn rebuild_visible_if_needed(&mut self) {
        if !self.visible_dirty {
            return;
        }

        let filter_lc = self.active_filter().trim().to_lowercase();
        let has_filter = !filter_lc.is_empty();

        self.visible_indices.clear();
        self.visible_indices.reserve(
            self.snapshot
                .rows
                .len()
                .saturating_sub(self.visible_indices.len()),
        );

        for (idx, row) in self.snapshot.rows.iter().enumerate() {
            let matches = if !has_filter {
                true
            } else {
                row.process_lc.contains(&filter_lc) || row.pid.to_string().contains(&filter_lc)
            };

            if matches {
                self.visible_indices.push(idx);
            }
        }

        self.visible_indices.sort_by(|a, b| {
            let ra = &self.snapshot.rows[*a];
            let rb = &self.snapshot.rows[*b];

            match self.sort {
                SortKey::Power => rb
                    .power_num
                    .partial_cmp(&ra.power_num)
                    .unwrap_or(Ordering::Equal),
                SortKey::Cpu => rb
                    .cpu_num
                    .partial_cmp(&ra.cpu_num)
                    .unwrap_or(Ordering::Equal),
                SortKey::Mem => rb
                    .mem_num
                    .partial_cmp(&ra.mem_num)
                    .unwrap_or(Ordering::Equal),
            }
            .then_with(|| ra.process.cmp(&rb.process))
        });

        self.visible_dirty = false;
    }

    fn visible_len(&mut self) -> usize {
        self.rebuild_visible_if_needed();
        self.visible_indices.len()
    }

    fn normalize_selection(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }

        if self.selected >= visible_len {
            self.selected = visible_len - 1;
        }

        if self.scroll > self.selected {
            self.scroll = self.selected;
        }
    }

    fn record_history(&mut self) {
        self.power_history.push(self.snapshot.total_power);
        self.cpu_history.push(self.snapshot.total_cpu);

        if self.power_history.len() > HISTORY_LIMIT {
            let extra = self.power_history.len() - HISTORY_LIMIT;
            self.power_history.drain(0..extra);
        }
        if self.cpu_history.len() > HISTORY_LIMIT {
            let extra = self.cpu_history.len() - HISTORY_LIMIT;
            self.cpu_history.drain(0..extra);
        }
    }

    fn set_sort(&mut self, sort: SortKey) {
        self.sort = sort;
        self.mark_visible_dirty();
        self.selected = 0;
        self.scroll = 0;
        let visible_len = self.visible_len();
        self.normalize_selection(visible_len);
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible_len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }

        let max = (len - 1) as isize;
        let next = (self.selected as isize + delta).clamp(0, max) as usize;
        self.selected = next;
    }

    fn select_top(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    fn select_bottom(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = len - 1;
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    fn start_filter_input(&mut self) {
        self.filter_input = Some(self.filter_query.clone());
        self.mark_visible_dirty();
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let Some(buf) = self.filter_input.as_mut() else {
            return;
        };

        let mut touched_filter = false;

        match key.code {
            KeyCode::Esc => {
                self.filter_input = None;
                self.selected = 0;
                self.scroll = 0;
                touched_filter = true;
            }
            KeyCode::Enter => {
                self.filter_query = buf.clone();
                self.filter_input = None;
                self.selected = 0;
                self.scroll = 0;
                touched_filter = true;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.selected = 0;
                self.scroll = 0;
                touched_filter = true;
            }
            KeyCode::Char(ch) => {
                buf.push(ch);
                self.selected = 0;
                self.scroll = 0;
                touched_filter = true;
            }
            _ => {}
        }

        if touched_filter {
            self.mark_visible_dirty();
        }

        let visible_len = self.visible_len();
        self.normalize_selection(visible_len);
    }

    fn apply_snapshot(&mut self, next: Snapshot) {
        self.snapshot = next;
        self.last_error = None;
        self.loading = false;
        self.mark_visible_dirty();
        self.record_history();

        let visible_len = self.visible_len();
        self.normalize_selection(visible_len);
    }

    fn apply_refresh_error(&mut self, err: String) {
        self.loading = false;
        self.last_error = Some(format!("refresh failed: {err}"));
    }

    fn apply_collector_event(&mut self, event: CollectorEvent) {
        match event {
            CollectorEvent::Snapshot(next) => self.apply_snapshot(next),
            CollectorEvent::Error(err) => self.apply_refresh_error(err),
        }
    }

    fn display_filter_text(&self) -> String {
        if self.active_filter().is_empty() {
            "(none)".to_string()
        } else {
            self.active_filter().to_string()
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }

        if self.filter_input.is_some() {
            self.handle_filter_key(key);
            return false;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => self.select_top(),
            KeyCode::Char('G') => self.select_bottom(),
            KeyCode::Char('/') => self.start_filter_input(),
            KeyCode::Char('p') | KeyCode::Char('P') => self.set_sort(SortKey::Power),
            KeyCode::Char('c') | KeyCode::Char('C') => self.set_sort(SortKey::Cpu),
            KeyCode::Char('m') | KeyCode::Char('M') => self.set_sort(SortKey::Mem),
            KeyCode::Char(' ') => self.toggle_pause(),
            _ => {}
        }

        let visible_len = self.visible_len();
        self.normalize_selection(visible_len);
        false
    }
}

fn main() -> io::Result<()> {
    let dump_once = std::env::args().any(|a| a == "--dump-once");

    if dump_once {
        match fetch_snapshot() {
            Ok(snapshot) => {
                let mut rows = snapshot.rows;
                rows.sort_by(|a, b| {
                    b.power_num
                        .partial_cmp(&a.power_num)
                        .unwrap_or(Ordering::Equal)
                });
                for row in rows.into_iter().take(10) {
                    println!(
                        "{:>6}  {:<24}  {:>6}  {:>8}  {:>8}",
                        row.pid, row.process, row.cpu, row.mem, row.power
                    );
                }
                return Ok(());
            }
            Err(err) => {
                eprintln!("etop: failed to fetch top data: {err}");
                return Err(io::Error::other(format!("failed to fetch top data: {err}")));
            }
        }
    }

    run_tui()
}

fn run_tui() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = app_loop(&mut terminal);
    ratatui::restore();
    result
}

fn app_loop(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut app = App::new();
    let (event_tx, event_rx) = mpsc::channel::<CollectorEvent>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<CollectorCommand>();

    let collector = thread::spawn(move || collector_loop(event_tx, cmd_rx));

    let loop_result = (|| -> io::Result<()> {
        loop {
            drain_collector_events(&mut app, &event_rx);
            terminal.draw(|f| draw_ui(f, &mut app))?;

            if event::poll(REDRAW_EVERY)? {
                if let Event::Key(key) = event::read()? {
                    let was_paused = app.paused;
                    if app.handle_key(key) {
                        break;
                    }

                    if app.paused != was_paused {
                        let _ = cmd_tx.send(CollectorCommand::SetPaused(app.paused));
                    }
                }
            }
        }

        Ok(())
    })();

    let _ = cmd_tx.send(CollectorCommand::Stop);
    let _ = collector.join();

    loop_result
}

fn collector_loop(event_tx: Sender<CollectorEvent>, cmd_rx: Receiver<CollectorCommand>) {
    let mut paused = false;
    let mut next_fetch_start = Instant::now();

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            if handle_collector_command(cmd, &mut paused, &mut next_fetch_start) {
                return;
            }
        }

        if paused {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if handle_collector_command(cmd, &mut paused, &mut next_fetch_start) {
                        return;
                    }
                }
                Err(_) => return,
            }
            continue;
        }

        let now = Instant::now();
        if now < next_fetch_start {
            let wait_for = next_fetch_start - now;
            match cmd_rx.recv_timeout(wait_for) {
                Ok(cmd) => {
                    if handle_collector_command(cmd, &mut paused, &mut next_fetch_start) {
                        return;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            if handle_collector_command(cmd, &mut paused, &mut next_fetch_start) {
                return;
            }
        }

        if paused {
            continue;
        }

        let fetch_started = Instant::now();
        match fetch_snapshot() {
            Ok(snapshot) => {
                if event_tx.send(CollectorEvent::Snapshot(snapshot)).is_err() {
                    return;
                }
            }
            Err(err) => {
                if event_tx
                    .send(CollectorEvent::Error(err.to_string()))
                    .is_err()
                {
                    return;
                }
            }
        }

        next_fetch_start = fetch_started + REFRESH_EVERY;
    }
}

fn handle_collector_command(
    cmd: CollectorCommand,
    paused: &mut bool,
    next_fetch_start: &mut Instant,
) -> bool {
    match cmd {
        CollectorCommand::SetPaused(next) => {
            let was_paused = *paused;
            *paused = next;
            if was_paused && !*paused {
                *next_fetch_start = Instant::now();
            }
            false
        }
        CollectorCommand::Stop => true,
    }
}

fn drain_collector_events(app: &mut App, event_rx: &Receiver<CollectorEvent>) {
    while let Ok(event) = event_rx.try_recv() {
        app.apply_collector_event(event);
    }
}

fn panel_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_MUTED))
        .title_style(
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(COLOR_BG).fg(COLOR_FG))
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    let base_style = Style::default().bg(COLOR_BG).fg(COLOR_FG);
    frame.render_widget(Block::default().style(base_style), frame.area());

    app.rebuild_visible_if_needed();
    let visible_len = app.visible_indices.len();
    app.normalize_selection(visible_len);

    let selected_process = app
        .visible_indices
        .get(app.selected)
        .and_then(|idx| app.snapshot.rows.get(*idx))
        .cloned();

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(frame.area());

    let mode = if app.paused { "PAUSED" } else { "LIVE" };
    let mode_style = if app.paused {
        Style::default()
            .fg(COLOR_YELLOW)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(COLOR_GREEN)
            .add_modifier(Modifier::BOLD)
    };
    let load_state = if app.loading { "loading" } else { "ready" };
    let filter_display = app.display_filter_text();

    let top_bar = Paragraph::new(Line::from(vec![
        Span::styled(
            "etop",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(COLOR_MUTED)),
        Span::styled(mode, mode_style),
        Span::styled(format!(" • {load_state}"), Style::default().fg(COLOR_MUTED)),
        Span::styled(" • sort:", Style::default().fg(COLOR_MUTED)),
        Span::styled(app.sort.as_str(), Style::default().fg(COLOR_ACCENT)),
        Span::styled(" • filter:", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            filter_display.clone(),
            if filter_display == "(none)" {
                Style::default().fg(COLOR_MUTED)
            } else {
                Style::default().fg(COLOR_FG)
            },
        ),
        Span::styled(" • rows:", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{visible_len}/{}", app.snapshot.rows.len()),
            Style::default().fg(COLOR_FG),
        ),
    ]))
    .style(base_style);
    frame.render_widget(top_bar, layout[0]);

    let top = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(layout[1]);

    let stats = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Rows ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                format!("{visible_len}/{}", app.snapshot.rows.len()),
                Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Power ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                format!("{:.1}", app.snapshot.total_power),
                Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("CPU ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                format!("{:.1}%", app.snapshot.total_cpu),
                Style::default()
                    .fg(COLOR_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Filter ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                filter_display.clone(),
                if filter_display == "(none)" {
                    Style::default().fg(COLOR_MUTED)
                } else {
                    Style::default().fg(COLOR_FG)
                },
            ),
        ]),
    ])
    .block(panel_block().title("Stats"))
    .wrap(Wrap { trim: true });
    frame.render_widget(stats, top[0]);

    let mut power_data: Vec<u64> = app
        .power_history
        .iter()
        .map(|v| (v.max(0.0) * 10.0) as u64)
        .collect();
    if power_data.is_empty() {
        power_data.push(0);
    }

    let mut cpu_data: Vec<u64> = app
        .cpu_history
        .iter()
        .map(|v| (v.max(0.0) * 10.0) as u64)
        .collect();
    if cpu_data.is_empty() {
        cpu_data.push(0);
    }

    let signals_block = panel_block().title("Signals");
    let signals_inner = signals_block.inner(top[1]);
    frame.render_widget(signals_block, top[1]);

    let signal_rows =
        Layout::vertical([Constraint::Length(2), Constraint::Min(2)]).split(signals_inner);

    let power_row =
        Layout::horizontal([Constraint::Length(16), Constraint::Min(1)]).split(signal_rows[0]);
    let power_label = Paragraph::new(Line::from(vec![
        Span::styled("PWR ", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{:.1}", app.snapshot.total_power),
            Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(power_label, power_row[0]);
    let power_chart = Sparkline::default()
        .data(&power_data)
        .style(Style::default().fg(COLOR_RED).bg(COLOR_BG));
    frame.render_widget(power_chart, power_row[1]);

    let cpu_row =
        Layout::horizontal([Constraint::Length(16), Constraint::Min(1)]).split(signal_rows[1]);
    let cpu_label = Paragraph::new(Line::from(vec![
        Span::styled("CPU ", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{:.1}%", app.snapshot.total_cpu),
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(cpu_label, cpu_row[0]);
    let cpu_chart = Sparkline::default()
        .data(&cpu_data)
        .style(Style::default().fg(COLOR_ACCENT).bg(COLOR_BG));
    frame.render_widget(cpu_chart, cpu_row[1]);

    let main = Layout::horizontal([Constraint::Percentage(78), Constraint::Percentage(22)])
        .split(layout[2]);
    let side = Layout::vertical([Constraint::Length(9), Constraint::Min(4)]).split(main[1]);

    let table_area = main[0];
    let rows_visible = table_area.height.saturating_sub(3) as usize;

    if rows_visible > 0 && visible_len > 0 {
        if app.selected < app.scroll {
            app.scroll = app.selected;
        }
        if app.selected >= app.scroll + rows_visible {
            app.scroll = app.selected + 1 - rows_visible;
        }
    } else {
        app.scroll = 0;
    }

    let start = app.scroll.min(visible_len);
    let end = if rows_visible == 0 {
        start
    } else {
        (start + rows_visible).min(visible_len)
    };

    let rows = app.visible_indices[start..end].iter().map(|idx| {
        let r = &app.snapshot.rows[*idx];
        Row::new([
            Cell::from(r.pid.to_string()),
            Cell::from(r.process.clone()),
            Cell::from(r.power.clone()).style(Style::default().fg(COLOR_RED)),
            Cell::from(r.cpu.clone()).style(Style::default().fg(COLOR_ACCENT)),
            Cell::from(r.mem.clone()).style(Style::default().fg(COLOR_YELLOW)),
        ])
        .style(Style::default().fg(COLOR_FG))
    });

    let header_row = Row::new([
        Cell::from("PID"),
        Cell::from("PROCESS"),
        Cell::from("POWER"),
        Cell::from("CPU"),
        Cell::from("MEM"),
    ])
    .style(
        Style::default()
            .fg(COLOR_ACCENT)
            .bg(COLOR_SELECTED_BG)
            .add_modifier(Modifier::BOLD),
    );

    let table_title = if app.filter_input.is_some() {
        format!(
            "Processes {visible_len}/{} • filtering: {}",
            app.snapshot.rows.len(),
            app.active_filter()
        )
    } else if app.filter_query.is_empty() {
        format!(
            "Processes {visible_len}/{} • {} ↓",
            app.snapshot.rows.len(),
            app.sort.as_str()
        )
    } else {
        format!(
            "Processes {visible_len}/{} • {} ↓ • {}",
            app.snapshot.rows.len(),
            app.sort.as_str(),
            app.filter_query
        )
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Percentage(56),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(11),
        ],
    )
    .header(header_row)
    .block(panel_block().title(table_title))
    .column_spacing(1)
    .style(Style::default().fg(COLOR_FG).bg(COLOR_BG))
    .row_highlight_style(
        Style::default()
            .bg(COLOR_SELECTED_BG)
            .fg(COLOR_FG)
            .add_modifier(Modifier::BOLD),
    );

    let selected_in_window = if visible_len == 0 {
        None
    } else {
        Some(app.selected.saturating_sub(start))
    };
    let mut table_state = TableState::default().with_selected(selected_in_window);
    frame.render_stateful_widget(table, table_area, &mut table_state);

    let detail_lines = if let Some(row) = selected_process {
        let power_share = if app.snapshot.total_power > 0.0 {
            (row.power_num / app.snapshot.total_power) * 100.0
        } else {
            0.0
        };
        let cpu_share = if app.snapshot.total_cpu > 0.0 {
            (row.cpu_num / app.snapshot.total_cpu) * 100.0
        } else {
            0.0
        };

        vec![
            Line::from(Span::styled(
                row.process.clone(),
                Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("pid ", Style::default().fg(COLOR_MUTED)),
                Span::styled(row.pid.to_string(), Style::default().fg(COLOR_FG)),
            ]),
            Line::from(vec![
                Span::styled("pwr ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    row.power.clone(),
                    Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  cpu ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    row.cpu.clone(),
                    Style::default()
                        .fg(COLOR_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("mem ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    row.mem.clone(),
                    Style::default()
                        .fg(COLOR_YELLOW)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("rank ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    format!("#{} / {}", app.selected + 1, visible_len),
                    Style::default().fg(COLOR_FG),
                ),
            ]),
            Line::from(vec![
                Span::styled("share ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    format!("{power_share:.1}% pwr"),
                    Style::default().fg(COLOR_RED),
                ),
                Span::styled("  ", Style::default().fg(COLOR_MUTED)),
                Span::styled(
                    format!("{cpu_share:.1}% cpu"),
                    Style::default().fg(COLOR_ACCENT),
                ),
            ]),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "No matching process",
                Style::default().fg(COLOR_MUTED),
            )),
            Line::from(Span::styled(
                "Adjust filter or wait for refresh.",
                Style::default().fg(COLOR_MUTED),
            )),
        ]
    };

    let detail = Paragraph::new(detail_lines)
        .block(panel_block().title("Selected"))
        .wrap(Wrap { trim: true });
    frame.render_widget(detail, side[0]);

    let selection_state = if visible_len == 0 {
        "none".to_string()
    } else {
        format!("{}/{}", app.selected + 1, visible_len)
    };

    let mut context_lines = vec![
        Line::from(vec![
            Span::styled("mode ", Style::default().fg(COLOR_MUTED)),
            Span::styled(mode, mode_style),
        ]),
        Line::from(vec![
            Span::styled("load ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                load_state,
                if app.loading {
                    Style::default().fg(COLOR_YELLOW)
                } else {
                    Style::default().fg(COLOR_GREEN)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("sort ", Style::default().fg(COLOR_MUTED)),
            Span::styled(app.sort.as_str(), Style::default().fg(COLOR_ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(COLOR_MUTED)),
            Span::styled(filter_display, Style::default().fg(COLOR_FG)),
        ]),
        Line::from(vec![
            Span::styled("sel ", Style::default().fg(COLOR_MUTED)),
            Span::styled(selection_state, Style::default().fg(COLOR_FG)),
        ]),
    ];

    if let Some(error) = app.last_error.as_deref() {
        context_lines.push(Line::from(vec![
            Span::styled("error ", Style::default().fg(COLOR_MUTED)),
            Span::styled(error, Style::default().fg(COLOR_RED)),
        ]));
    } else {
        context_lines.push(Line::from(vec![
            Span::styled("error ", Style::default().fg(COLOR_MUTED)),
            Span::styled("none", Style::default().fg(COLOR_MUTED)),
        ]));
    }

    let context = Paragraph::new(context_lines)
        .block(panel_block().title("Session"))
        .wrap(Wrap { trim: true });
    frame.render_widget(context, side[1]);

    let mut footer_line = vec![
        Span::styled("q", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" quit  ", Style::default().fg(COLOR_MUTED)),
        Span::styled("j/k ↑/↓", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" move  ", Style::default().fg(COLOR_MUTED)),
        Span::styled("g/G", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" top/btm  ", Style::default().fg(COLOR_MUTED)),
        Span::styled("/", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" filter  ", Style::default().fg(COLOR_MUTED)),
        Span::styled("p/c/m", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" sort  ", Style::default().fg(COLOR_MUTED)),
        Span::styled("space", Style::default().fg(COLOR_ACCENT)),
        Span::styled(" pause", Style::default().fg(COLOR_MUTED)),
    ];

    if let Some(error) = app.last_error.as_deref() {
        footer_line.push(Span::styled("  •  ", Style::default().fg(COLOR_MUTED)));
        footer_line.push(Span::styled(
            format!("error: {error}"),
            Style::default().fg(COLOR_RED),
        ));
    }

    let footer = Paragraph::new(Line::from(footer_line))
        .block(panel_block().title("Controls"))
        .wrap(Wrap { trim: false });
    frame.render_widget(footer, layout[3]);
}

fn fetch_snapshot() -> io::Result<Snapshot> {
    let output = Command::new(TOP_BIN).args(TOP_ARGS).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "top command failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows = parse_second_sample(&stdout);
    let total_power = rows.iter().map(|r| r.power_num).sum::<f64>();
    let total_cpu = rows.iter().map(|r| r.cpu_num).sum::<f64>();

    Ok(Snapshot {
        rows,
        total_power,
        total_cpu,
    })
}

fn parse_second_sample(raw: &str) -> Vec<ProcRow> {
    // Keep macOS top "second sample" semantics by resetting rows each time we hit a PID header.
    let mut rows = Vec::new();
    let mut in_table = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("PID") {
            rows.clear();
            in_table = true;
            continue;
        }

        if !in_table {
            continue;
        }

        let first = trimmed.chars().next().unwrap_or(' ');
        if !first.is_ascii_digit() {
            continue;
        }

        if let Some(row) = parse_row(trimmed) {
            rows.push(row);
        }
    }

    rows
}

fn parse_row(line: &str) -> Option<ProcRow> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }

    let pid = parts[0].parse::<i32>().ok()?;

    let power = parts.last()?.to_string();
    let mem = parts.get(parts.len().saturating_sub(2))?.to_string();
    let cpu = parts.get(parts.len().saturating_sub(3))?.to_string();
    let process = parts[1..parts.len().saturating_sub(3)].join(" ");
    let process_lc = process.to_lowercase();

    Some(ProcRow {
        pid,
        process,
        process_lc,
        cpu_num: parse_numeric_value(&cpu),
        mem_num: parse_mem_value(&mem),
        power_num: parse_numeric_value(&power),
        cpu,
        mem,
        power,
    })
}

fn parse_numeric_value(s: &str) -> f64 {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    cleaned.parse::<f64>().unwrap_or(0.0)
}

fn parse_mem_value(s: &str) -> f64 {
    let mut number = String::new();
    let mut suffix = None;

    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
        } else if ch.is_ascii_alphabetic() {
            suffix = Some(ch.to_ascii_uppercase());
            break;
        }
    }

    let base = number.parse::<f64>().unwrap_or(0.0);
    let multiplier = match suffix {
        Some('K') => 1024.0,
        Some('M') => 1024.0 * 1024.0,
        Some('G') => 1024.0 * 1024.0 * 1024.0,
        Some('T') => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };

    base * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_second_sample_keeps_only_last_pid_block() {
        let raw = r#"
Processes: 123 total
PID COMMAND %CPU MEM POWER
1 launchd 0.0 15M 0.2
2 Finder 1.2 87M 1.9

PID COMMAND %CPU MEM POWER
99 Safari 15.8 420M 12.3
100 Google Chrome Helper 3.1 118M 4.6
"#;

        let rows = parse_second_sample(raw);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![99, 100]);
        assert_eq!(rows[1].process, "Google Chrome Helper");
    }

    #[test]
    fn parse_row_supports_multi_word_process_name() {
        let row = parse_row("4242 Google Chrome Helper 7.4 512M 9.1").expect("row should parse");

        assert_eq!(row.pid, 4242);
        assert_eq!(row.process, "Google Chrome Helper");
        assert_eq!(row.cpu, "7.4");
        assert_eq!(row.mem, "512M");
        assert_eq!(row.power, "9.1");
        assert!((row.mem_num - (512.0 * 1024.0 * 1024.0)).abs() < 1.0);
    }

    #[test]
    fn parse_mem_value_handles_suffixes() {
        assert!((parse_mem_value("2K") - 2048.0).abs() < f64::EPSILON);
        assert!((parse_mem_value("2M") - (2.0 * 1024.0 * 1024.0)).abs() < 1.0);
        assert!((parse_mem_value("1.5G") - (1.5 * 1024.0 * 1024.0 * 1024.0)).abs() < 1.0);
    }
}
