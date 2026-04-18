use std::{
    cmp::Ordering,
    io,
    process::Command,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    DefaultTerminal,
};

const TOP_CMD: &str = "top -l 2 -o power -stats pid,command,cpu,mem,power";
const REFRESH_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct ProcRow {
    pid: i32,
    process: String,
    cpu: String,
    mem: String,
    power: String,
    power_num: f64,
}

#[derive(Debug, Clone)]
struct Snapshot {
    rows: Vec<ProcRow>,
}

fn main() -> io::Result<()> {
    let dump_once = std::env::args().any(|a| a == "--dump-once");

    if dump_once {
        match fetch_snapshot() {
            Ok(snapshot) => {
                for row in snapshot.rows.into_iter().take(10) {
                    println!(
                        "{:>6}  {:<24}  {:>6}  {:>8}  {:>8}",
                        row.pid, row.process, row.cpu, row.mem, row.power
                    );
                }
                return Ok(());
            }
            Err(err) => {
                eprintln!("etop: failed to fetch top data: {err}");
                return Ok(());
            }
        }
    }

    run_tui()
}

fn run_tui() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = ratatui::init();

    let result = app_loop(&mut terminal);

    ratatui::restore();
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
}

fn app_loop(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut snapshot = fetch_snapshot().unwrap_or_else(|_| Snapshot { rows: vec![] });
    let mut last_error: Option<String> = None;
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|f| draw_ui(f, &snapshot, last_error.as_deref()))?;

        let elapsed = last_refresh.elapsed();
        let timeout = REFRESH_EVERY.saturating_sub(elapsed);

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                    break;
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH_EVERY {
            match fetch_snapshot() {
                Ok(next) => {
                    snapshot = next;
                    last_error = None;
                }
                Err(err) => {
                    last_error = Some(format!("refresh failed: {err}"));
                }
            }
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

fn draw_ui(frame: &mut Frame, snapshot: &Snapshot, last_error: Option<&str>) {
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(frame.area());

    let header = Paragraph::new("etop MVP — macOS top power view")
        .block(Block::default().borders(Borders::ALL).title("Header"));
    frame.render_widget(header, layout[0]);

    let header_row = Row::new([
        Cell::from("PID"),
        Cell::from("PROCESS"),
        Cell::from("POWER"),
        Cell::from("CPU"),
        Cell::from("MEM"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = snapshot.rows.iter().map(|r| {
        Row::new([
            Cell::from(r.pid.to_string()),
            Cell::from(r.process.clone()),
            Cell::from(r.power.clone()),
            Cell::from(r.cpu.clone()),
            Cell::from(r.mem.clone()),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Percentage(45),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(12),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Processes (sorted by power desc)"),
    )
    .column_spacing(1);

    frame.render_widget(table, layout[1]);

    let status = if let Some(err) = last_error {
        format!(
            "src: `{TOP_CMD}` | refresh: {}s | sort: power desc | q: quit | {err}",
            REFRESH_EVERY.as_secs()
        )
    } else {
        format!(
            "src: `{TOP_CMD}` | refresh: {}s | sort: power desc | q: quit",
            REFRESH_EVERY.as_secs()
        )
    };

    let footer = Paragraph::new(status)
        .block(Block::default().borders(Borders::ALL).title("Footer"));
    frame.render_widget(footer, layout[2]);
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

    Ok(Snapshot { rows })
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

    rows.sort_by(|a, b| b.power_num.partial_cmp(&a.power_num).unwrap_or(Ordering::Equal));
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
        cpu,
        mem,
        power_num: parse_power_value(&power),
        power,
    })
}

fn parse_power_value(s: &str) -> f64 {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    cleaned.parse::<f64>().unwrap_or(0.0)
}
