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
    widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState},
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
const REFRESH_EVERY: Duration = Duration::from_secs(1);
const REDRAW_EVERY: Duration = Duration::from_millis(120);
const HISTORY_LIMIT: usize = 90;

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

fn draw_ui(frame: &mut Frame, app: &mut App) {
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(8),
        Constraint::Min(8),
        Constraint::Length(4),
    ])
    .split(frame.area());

    let header = Paragraph::new("etop — macOS power/process monitor")
        .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(header, layout[0]);

    let mid = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(layout[1]);
    let charts =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(mid[1]);

    let mode = if app.paused { "paused" } else { "live" };
    let load_state = if app.loading { "loading" } else { "ready" };
    let filter_text = app.display_filter_text();

    let summary = Paragraph::new(format!(
        "mode: {mode} ({load_state})\nrows: {}\nsort: {} desc\nagg power: {:.1}\nagg cpu: {:.1}%\nfilter: {}",
        app.snapshot.rows.len(),
        app.sort.as_str(),
        app.snapshot.total_power,
        app.snapshot.total_cpu,
        filter_text,
    ))
    .block(Block::default().borders(Borders::ALL).title("Stats"));
    frame.render_widget(summary, mid[0]);

    let power_data: Vec<u64> = if app.power_history.is_empty() {
        vec![0]
    } else {
        app.power_history
            .iter()
            .map(|v| (v.max(0.0) * 10.0) as u64)
            .collect()
    };

    let cpu_data: Vec<u64> = if app.cpu_history.is_empty() {
        vec![0]
    } else {
        app.cpu_history
            .iter()
            .map(|v| (v.max(0.0) * 10.0) as u64)
            .collect()
    };

    let power_chart = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Aggregate power history"),
        )
        .data(&power_data)
        .style(Style::default().fg(Color::LightRed));
    frame.render_widget(power_chart, charts[0]);

    let cpu_chart = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Aggregate CPU history"),
        )
        .data(&cpu_data)
        .style(Style::default().fg(Color::LightBlue));
    frame.render_widget(cpu_chart, charts[1]);

    app.rebuild_visible_if_needed();
    let visible_len = app.visible_indices.len();
    app.normalize_selection(visible_len);

    let table_area = layout[2];
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

    let visible = &app.visible_indices;

    let rows = visible[start..end].iter().map(|idx| {
        let r = &app.snapshot.rows[*idx];
        Row::new([
            Cell::from(r.pid.to_string()),
            Cell::from(r.process.clone()),
            Cell::from(r.power.clone()),
            Cell::from(r.cpu.clone()),
            Cell::from(r.mem.clone()),
        ])
    });

    let header_row = Row::new([
        Cell::from("PID"),
        Cell::from("PROCESS"),
        Cell::from("POWER"),
        Cell::from("CPU"),
        Cell::from("MEM"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let title = if app.filter_input.is_some() {
        format!("Processes (filtering: {})", app.active_filter())
    } else {
        format!("Processes (sort: {} desc)", app.sort.as_str())
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Percentage(48),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(12),
        ],
    )
    .header(header_row)
    .block(Block::default().borders(Borders::ALL).title(title))
    .column_spacing(1)
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let selected_in_window = if visible.is_empty() {
        None
    } else {
        Some(app.selected.saturating_sub(start))
    };
    let mut table_state = TableState::default().with_selected(selected_in_window);
    frame.render_stateful_widget(table, table_area, &mut table_state);

    let footer_text = if let Some(error) = app.last_error.as_deref() {
        format!(
            "keys: q quit | j/k,↑/↓ move | g/G top/bottom | / filter | Enter apply | Esc cancel | p/c/m sort | space pause/resume\nerror: {}",
            error
        )
    } else {
        "keys: q quit | j/k,↑/↓ move | g/G top/bottom | / filter | Enter apply | Esc cancel | p/c/m sort | space pause/resume".to_string()
    };

    let footer =
        Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL).title("Controls"));
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
