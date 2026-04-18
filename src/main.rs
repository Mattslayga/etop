use std::{
    cmp::Ordering,
    io,
    process::Command,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    prelude::*,
    widgets::{
        Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState,
    },
    DefaultTerminal,
};

const TOP_CMD: &str = "top -l 2 -o power -stats pid,command,cpu,mem,power";
const REFRESH_EVERY: Duration = Duration::from_secs(2);
const HISTORY_LIMIT: usize = 90;

#[derive(Debug, Clone)]
struct ProcRow {
    pid: i32,
    process: String,
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

struct App {
    snapshot: Snapshot,
    last_error: Option<String>,
    last_refresh: Instant,
    paused: bool,
    sort: SortKey,
    selected: usize,
    scroll: usize,
    filter_query: String,
    filter_input: Option<String>,
    power_history: Vec<f64>,
    cpu_history: Vec<f64>,
}

impl App {
    fn new() -> Self {
        let (snapshot, last_error) = match fetch_snapshot() {
            Ok(snapshot) => (snapshot, None),
            Err(err) => (
                Snapshot::default(),
                Some(format!("initial fetch failed: {err}")),
            ),
        };

        let mut app = Self {
            snapshot,
            last_error,
            last_refresh: Instant::now(),
            paused: false,
            sort: SortKey::Power,
            selected: 0,
            scroll: 0,
            filter_query: String::new(),
            filter_input: None,
            power_history: Vec::new(),
            cpu_history: Vec::new(),
        };

        app.record_history();
        app.normalize_selection(app.visible_len());
        app
    }

    fn active_filter(&self) -> &str {
        self.filter_input
            .as_deref()
            .unwrap_or(self.filter_query.as_str())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .snapshot
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                let q = self.active_filter().trim();
                if q.is_empty() {
                    return true;
                }

                let q = q.to_lowercase();
                row.process.to_lowercase().contains(&q) || row.pid.to_string().contains(&q)
            })
            .map(|(idx, _)| idx)
            .collect();

        indices.sort_by(|a, b| {
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

        indices
    }

    fn visible_len(&self) -> usize {
        self.visible_indices().len()
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
        self.selected = 0;
        self.scroll = 0;
        self.normalize_selection(self.visible_len());
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
        if !self.paused {
            self.last_refresh = Instant::now() - REFRESH_EVERY;
        }
    }

    fn start_filter_input(&mut self) {
        self.filter_input = Some(self.filter_query.clone());
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let Some(buf) = self.filter_input.as_mut() else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                self.filter_input = None;
                self.selected = 0;
                self.scroll = 0;
            }
            KeyCode::Enter => {
                self.filter_query = buf.clone();
                self.filter_input = None;
                self.selected = 0;
                self.scroll = 0;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.selected = 0;
                self.scroll = 0;
            }
            KeyCode::Char(ch) => {
                buf.push(ch);
                self.selected = 0;
                self.scroll = 0;
            }
            _ => {}
        }

        self.normalize_selection(self.visible_len());
    }

    fn refresh_if_due(&mut self) {
        if self.paused || self.last_refresh.elapsed() < REFRESH_EVERY {
            return;
        }

        match fetch_snapshot() {
            Ok(next) => {
                self.snapshot = next;
                self.last_error = None;
                self.record_history();
            }
            Err(err) => {
                self.last_error = Some(format!("refresh failed: {err}"));
            }
        }

        self.last_refresh = Instant::now();
        self.normalize_selection(self.visible_len());
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

        self.normalize_selection(self.visible_len());
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

    loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        let timeout = if app.paused {
            Duration::from_millis(200)
        } else {
            REFRESH_EVERY.saturating_sub(app.last_refresh.elapsed())
        };

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key) {
                    break;
                }
            }
        }

        app.refresh_if_due();
    }

    Ok(())
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(9),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .split(frame.area());

    let header = Paragraph::new("etop — macOS power/process monitor")
        .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(header, layout[0]);

    let mid = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(layout[1]);
    let charts = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(mid[1]);

    let mode = if app.paused { "paused" } else { "live" };
    let filter_display = app.active_filter();
    let filter_text = if filter_display.is_empty() {
        "(none)".to_string()
    } else {
        filter_display.to_string()
    };

    let summary = Paragraph::new(format!(
        "mode: {mode}\nrows: {}\nsort: {} desc\nagg power: {:.1}\nagg cpu: {:.1}%\nfilter: {}",
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

    let visible = app.visible_indices();
    app.normalize_selection(visible.len());

    let table_area = layout[2];
    let rows_visible = table_area.height.saturating_sub(3) as usize;

    if rows_visible > 0 && !visible.is_empty() {
        if app.selected < app.scroll {
            app.scroll = app.selected;
        }
        if app.selected >= app.scroll + rows_visible {
            app.scroll = app.selected + 1 - rows_visible;
        }
    } else {
        app.scroll = 0;
    }

    let start = app.scroll.min(visible.len());
    let end = if rows_visible == 0 {
        start
    } else {
        (start + rows_visible).min(visible.len())
    };

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

    let refresh_state = if app.paused {
        "paused"
    } else {
        "refreshing"
    };
    let status_error = app
        .last_error
        .as_deref()
        .map(|e| format!(" | {e}"))
        .unwrap_or_default();

    let footer = Paragraph::new(format!(
        "q quit | j/k,↑/↓ move | g/G top/bottom | / filter (Enter apply, Esc cancel) | p/c/m sort | space pause ({refresh_state}){status_error}"
    ))
    .block(Block::default().borders(Borders::ALL).title("Controls"));
    frame.render_widget(footer, layout[3]);
}

fn fetch_snapshot() -> io::Result<Snapshot> {
    let output = Command::new("sh").arg("-c").arg(TOP_CMD).output()?;
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
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("PID"));

    let Some(start_idx) = start else {
        return Vec::new();
    };

    let mut rows = Vec::new();

    for line in lines.iter().skip(start_idx + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
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

    Some(ProcRow {
        pid,
        process,
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
