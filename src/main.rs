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
    "pid,command,power",
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
    power: String,
    power_num: f64,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    rows: Vec<ProcRow>,
    total_power: f64,
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
    show_details: bool,
    selected: usize,
    scroll: usize,
    filter_query: String,
    filter_input: Option<String>,
    power_history: Vec<f64>,
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
            show_details: false,
            selected: 0,
            scroll: 0,
            filter_query: String::new(),
            filter_input: None,
            power_history: Vec::new(),
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

            rb.power_num
                .partial_cmp(&ra.power_num)
                .unwrap_or(Ordering::Equal)
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

        if self.power_history.len() > HISTORY_LIMIT {
            let extra = self.power_history.len() - HISTORY_LIMIT;
            self.power_history.drain(0..extra);
        }
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

    fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
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
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Enter => self.toggle_details(),
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
                    println!("{:>6}  {:<32}  {:>8}", row.pid, row.process, row.power);
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
        Constraint::Percentage(34),
        Constraint::Min(8),
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
    let load_style = if app.loading {
        Style::default().fg(COLOR_YELLOW)
    } else {
        Style::default().fg(COLOR_GREEN)
    };

    let filter_display = app.display_filter_text();
    let details_state = if app.show_details {
        "details:on"
    } else {
        "details:off"
    };

    let mut top_spans = vec![
        Span::styled(
            "etop",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(COLOR_MUTED)),
        Span::styled(mode, mode_style),
        Span::styled(" • ", Style::default().fg(COLOR_MUTED)),
        Span::styled(load_state, load_style),
        Span::styled(" • rows:", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{visible_len}/{}", app.snapshot.rows.len()),
            Style::default().fg(COLOR_FG),
        ),
        Span::styled(" • power:", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{:.1}", app.snapshot.total_power),
            Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" • filter:", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            filter_display,
            if app.active_filter().is_empty() {
                Style::default().fg(COLOR_MUTED)
            } else {
                Style::default().fg(COLOR_FG)
            },
        ),
        Span::styled(" • ", Style::default().fg(COLOR_MUTED)),
        Span::styled(details_state, Style::default().fg(COLOR_ACCENT)),
    ];

    if let Some(error) = app.last_error.as_deref() {
        top_spans.push(Span::styled(" • ", Style::default().fg(COLOR_MUTED)));
        top_spans.push(Span::styled(
            format!("error: {error}"),
            Style::default().fg(COLOR_RED),
        ));
    }

    let top_bar = Paragraph::new(Line::from(top_spans))
        .style(base_style)
        .wrap(Wrap { trim: false });
    frame.render_widget(top_bar, layout[0]);

    let mut power_data: Vec<u64> = app
        .power_history
        .iter()
        .map(|v| (v.max(0.0) * 10.0) as u64)
        .collect();
    if power_data.is_empty() {
        power_data.push(0);
    }

    let graph_title = if app.filter_input.is_some() {
        "Power history • / editing • Enter apply • Esc cancel".to_string()
    } else {
        "Power history • / filter • Enter details • space pause • q quit".to_string()
    };

    let graph_block = panel_block().title(graph_title);
    let graph_inner = graph_block.inner(layout[1]);
    frame.render_widget(graph_block, layout[1]);

    let graph_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(graph_inner);
    let graph_label = Paragraph::new(Line::from(vec![
        Span::styled("POWER ", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{:.1}", app.snapshot.total_power),
            Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  • {} points", power_data.len()),
            Style::default().fg(COLOR_MUTED),
        ),
    ]));
    frame.render_widget(graph_label, graph_rows[0]);

    let power_chart = Sparkline::default()
        .data(&power_data)
        .style(Style::default().fg(COLOR_RED).bg(COLOR_BG));
    frame.render_widget(power_chart, graph_rows[1]);

    let table_region = layout[2];
    let (detail_area, rows_area) = if app.show_details {
        let min_rows: u16 = 6;
        let max_detail = table_region.height.saturating_sub(min_rows);

        if max_detail >= 4 {
            let preferred = (table_region.height * 35) / 100;
            let detail_height = preferred.clamp(4, max_detail);
            let split =
                Layout::vertical([Constraint::Length(detail_height), Constraint::Min(min_rows)])
                    .split(table_region);
            (Some(split[0]), split[1])
        } else {
            (None, table_region)
        }
    } else {
        (None, table_region)
    };

    if let Some(detail_rect) = detail_area {
        let detail_lines = if let Some(row) = selected_process {
            let power_share = if app.snapshot.total_power > 0.0 {
                (row.power_num / app.snapshot.total_power) * 100.0
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
                        format!("{power_share:.1}% of total"),
                        Style::default().fg(COLOR_RED),
                    ),
                ]),
                Line::from(Span::styled(
                    "Enter to hide details",
                    Style::default().fg(COLOR_MUTED),
                )),
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
                Line::from(Span::styled(
                    "Enter to hide details",
                    Style::default().fg(COLOR_MUTED),
                )),
            ]
        };

        let detail = Paragraph::new(detail_lines)
            .block(panel_block().title("Selected • Enter hide"))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, detail_rect);
    }

    let rows_visible = rows_area.height.saturating_sub(3) as usize;

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
        ])
        .style(Style::default().fg(COLOR_FG))
    });

    let header_row = Row::new([
        Cell::from("PID"),
        Cell::from("PROCESS"),
        Cell::from("POWER"),
    ])
    .style(
        Style::default()
            .fg(COLOR_ACCENT)
            .bg(COLOR_SELECTED_BG)
            .add_modifier(Modifier::BOLD),
    );

    let detail_hint = if app.show_details {
        "Enter hide details"
    } else {
        "Enter show details"
    };

    let table_title = if app.filter_input.is_some() {
        format!(
            "Processes {visible_len}/{} • filtering: {} • Enter apply",
            app.snapshot.rows.len(),
            app.active_filter()
        )
    } else if app.filter_query.is_empty() {
        format!(
            "Processes {visible_len}/{} • power ↓ • j/k ↑/↓ • g/G • {detail_hint}",
            app.snapshot.rows.len(),
        )
    } else {
        format!(
            "Processes {visible_len}/{} • power ↓ • filter: {} • {detail_hint}",
            app.snapshot.rows.len(),
            app.filter_query,
        )
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Percentage(70),
            Constraint::Length(10),
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
    frame.render_stateful_widget(table, rows_area, &mut table_state);
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

    Ok(Snapshot { rows, total_power })
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
    if parts.len() < 3 {
        return None;
    }

    let pid = parts[0].parse::<i32>().ok()?;
    let power = parts.last()?.to_string();
    let process = parts[1..parts.len().saturating_sub(1)].join(" ");
    let process_lc = process.to_lowercase();

    Some(ProcRow {
        pid,
        process,
        process_lc,
        power_num: parse_numeric_value(&power),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_second_sample_keeps_only_last_pid_block() {
        let raw = r#"
Processes: 123 total
PID COMMAND POWER
1 launchd 0.2
2 Finder 1.9

PID COMMAND POWER
99 Safari 12.3
100 Google Chrome Helper 4.6
"#;

        let rows = parse_second_sample(raw);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![99, 100]);
        assert_eq!(rows[1].process, "Google Chrome Helper");
    }

    #[test]
    fn parse_row_supports_multi_word_process_name() {
        let row = parse_row("4242 Google Chrome Helper 9.1").expect("row should parse");

        assert_eq!(row.pid, 4242);
        assert_eq!(row.process, "Google Chrome Helper");
        assert_eq!(row.power, "9.1");
        assert!((row.power_num - 9.1).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_row_rejects_missing_columns() {
        assert!(parse_row("123 onlypid").is_none());
    }
}
