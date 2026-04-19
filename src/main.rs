use std::{
    cmp::Ordering,
    io::{self, BufRead, BufReader},
    process::{Child, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
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
const COLLECTOR_POLL_EVERY: Duration = Duration::from_millis(200);
const SAMPLER_RESTART_BACKOFF: Duration = Duration::from_secs(1);
const SAMPLER_QUEUE_CAPACITY: usize = 8;
const HISTORY_LIMIT: usize = 240;

const COLOR_BG: Color = Color::Rgb(0x28, 0x2c, 0x34);
const COLOR_FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
const COLOR_ACCENT: Color = Color::Rgb(0x61, 0xaf, 0xef);
const COLOR_MUTED: Color = Color::Rgb(0x5c, 0x63, 0x70);
const COLOR_SELECTED_BG: Color = Color::Rgb(0x2c, 0x31, 0x3c);
const COLOR_GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
const COLOR_YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const COLOR_ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
const COLOR_RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);

const BRAILLE_5X5: [char; 25] = [
    ' ', '⢀', '⢠', '⢰', '⢸', '⡀', '⣀', '⣠', '⣰', '⣸', '⡄', '⣄', '⣤', '⣴', '⣼', '⡆', '⣆', '⣦', '⣶',
    '⣾', '⡇', '⣇', '⣧', '⣷', '⣿',
];

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

#[derive(Debug, Clone)]
struct PinnedProcess {
    pid: i32,
    process: String,
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

#[derive(Debug)]
enum SamplerEvent {
    Snapshot(Snapshot),
    Error(String),
    Ended,
}

struct SamplerRuntime {
    child: Child,
    events: Receiver<SamplerEvent>,
    reader: thread::JoinHandle<()>,
}

#[derive(Default)]
struct TopStreamParser {
    excluded_pids: Vec<i32>,
    in_table: bool,
    skipped_warmup: bool,
    rows: Vec<ProcRow>,
}

impl TopStreamParser {
    fn new(excluded_pids: Vec<i32>) -> Self {
        Self {
            excluded_pids,
            in_table: false,
            skipped_warmup: false,
            rows: Vec::new(),
        }
    }

    fn push_line(&mut self, line: &str) -> Result<Option<Snapshot>, String> {
        let trimmed = line.trim();

        if trimmed.starts_with("PID") {
            let finished = self.finish_frame();
            self.in_table = true;
            return Ok(finished);
        }

        if !self.in_table {
            return Ok(None);
        }

        if trimmed.is_empty() {
            return Ok(self.finish_frame());
        }

        let first = trimmed.chars().next().unwrap_or(' ');
        if !first.is_ascii_digit() {
            return Ok(self.finish_frame());
        }

        let row =
            parse_row(trimmed).ok_or_else(|| format!("unable to parse top row: {trimmed}"))?;

        if !self.excluded_pids.contains(&row.pid) {
            self.rows.push(row);
        }

        Ok(None)
    }

    fn finish_stream(&mut self) -> Option<Snapshot> {
        self.finish_frame()
    }

    fn finish_frame(&mut self) -> Option<Snapshot> {
        if !self.in_table {
            return None;
        }

        self.in_table = false;
        let rows = std::mem::take(&mut self.rows);

        if !self.skipped_warmup {
            self.skipped_warmup = true;
            return None;
        }

        Some(snapshot_from_rows(rows))
    }
}

struct App {
    snapshot: Snapshot,
    last_error: Option<String>,
    loading: bool,
    paused: bool,
    pinned: Option<PinnedProcess>,
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
            pinned: None,
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

    fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    fn toggle_pin(&mut self) {
        if self.pinned.is_some() {
            self.pinned = None;
            return;
        }

        self.rebuild_visible_if_needed();
        let Some(snapshot_idx) = self.visible_indices.get(self.selected).copied() else {
            return;
        };

        if let Some(row) = self.snapshot.rows.get(snapshot_idx) {
            self.pinned = Some(PinnedProcess {
                pid: row.pid,
                process: row.process.clone(),
            });
        }
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

    fn status_hint(&self) -> &'static str {
        if self.filter_input.is_some() {
            "filter edit: Enter apply • Esc cancel"
        } else if self.pinned.is_some() {
            "Enter unpin • / filter • space pause • q quit"
        } else {
            "j/k move • Enter pin • / filter • space pause • q quit"
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
            KeyCode::Char('j') | KeyCode::Down if !self.is_pinned() => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up if !self.is_pinned() => self.move_selection(-1),
            KeyCode::Char('g') if !self.is_pinned() => self.select_top(),
            KeyCode::Char('G') if !self.is_pinned() => self.select_bottom(),
            KeyCode::Char('/') => self.start_filter_input(),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Enter => self.toggle_pin(),
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
    let mut sampler: Option<SamplerRuntime> = None;
    let self_pid = i32::try_from(std::process::id()).ok();

    loop {
        if drain_collector_commands(&cmd_rx, &mut paused, &mut sampler) {
            return;
        }

        if sampler.is_none() {
            match start_sampler(self_pid) {
                Ok(next_sampler) => sampler = Some(next_sampler),
                Err(err) => {
                    if event_tx
                        .send(CollectorEvent::Error(format!(
                            "sampler start failed: {err}"
                        )))
                        .is_err()
                    {
                        return;
                    }

                    if wait_for_restart_or_stop(&cmd_rx, &mut paused, &mut sampler) {
                        return;
                    }

                    continue;
                }
            }
        }

        let sampler_events = match sampler.as_ref() {
            Some(runtime) => &runtime.events,
            None => continue,
        };

        match sampler_events.recv_timeout(COLLECTOR_POLL_EVERY) {
            Ok(SamplerEvent::Snapshot(snapshot)) => {
                if paused {
                    continue;
                }

                if event_tx.send(CollectorEvent::Snapshot(snapshot)).is_err() {
                    if let Some(runtime) = sampler.take() {
                        shutdown_sampler(runtime);
                    }
                    return;
                }
            }
            Ok(SamplerEvent::Error(err)) => {
                if event_tx
                    .send(CollectorEvent::Error(format!(
                        "sampler stream failed: {err}"
                    )))
                    .is_err()
                {
                    if let Some(runtime) = sampler.take() {
                        shutdown_sampler(runtime);
                    }
                    return;
                }

                if let Some(runtime) = sampler.take() {
                    shutdown_sampler(runtime);
                }

                if wait_for_restart_or_stop(&cmd_rx, &mut paused, &mut sampler) {
                    return;
                }
            }
            Ok(SamplerEvent::Ended) => {
                if event_tx
                    .send(CollectorEvent::Error(
                        "sampler process exited; restarting".to_string(),
                    ))
                    .is_err()
                {
                    if let Some(runtime) = sampler.take() {
                        shutdown_sampler(runtime);
                    }
                    return;
                }

                if let Some(runtime) = sampler.take() {
                    shutdown_sampler(runtime);
                }

                if wait_for_restart_or_stop(&cmd_rx, &mut paused, &mut sampler) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if event_tx
                    .send(CollectorEvent::Error(
                        "sampler channel disconnected; restarting".to_string(),
                    ))
                    .is_err()
                {
                    if let Some(runtime) = sampler.take() {
                        shutdown_sampler(runtime);
                    }
                    return;
                }

                if let Some(runtime) = sampler.take() {
                    shutdown_sampler(runtime);
                }

                if wait_for_restart_or_stop(&cmd_rx, &mut paused, &mut sampler) {
                    return;
                }
            }
        }
    }
}

fn start_sampler(self_pid: Option<i32>) -> io::Result<SamplerRuntime> {
    let sample_seconds = REFRESH_EVERY.as_secs().max(1).to_string();
    let mut child = Command::new(TOP_BIN)
        .arg("-l")
        .arg("0")
        .arg("-s")
        .arg(sample_seconds)
        .arg("-o")
        .arg("power")
        .arg("-stats")
        .arg("pid,command,power")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut excluded_pids = Vec::with_capacity(2);
    if let Ok(top_pid) = i32::try_from(child.id()) {
        excluded_pids.push(top_pid);
    }
    if let Some(pid) = self_pid {
        excluded_pids.push(pid);
    }

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("sampler stdout was not available"));
        }
    };

    let (event_tx, event_rx) = mpsc::sync_channel::<SamplerEvent>(SAMPLER_QUEUE_CAPACITY);
    let reader = thread::spawn(move || sampler_reader_loop(stdout, excluded_pids, event_tx));

    Ok(SamplerRuntime {
        child,
        events: event_rx,
        reader,
    })
}

fn sampler_reader_loop(
    stdout: ChildStdout,
    excluded_pids: Vec<i32>,
    event_tx: SyncSender<SamplerEvent>,
) {
    let mut parser = TopStreamParser::new(excluded_pids);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Some(snapshot) = parser.finish_stream() {
                    if event_tx.send(SamplerEvent::Snapshot(snapshot)).is_err() {
                        return;
                    }
                }
                let _ = event_tx.send(SamplerEvent::Ended);
                return;
            }
            Ok(_) => match parser.push_line(&line) {
                Ok(Some(snapshot)) => {
                    if event_tx.send(SamplerEvent::Snapshot(snapshot)).is_err() {
                        return;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = event_tx.send(SamplerEvent::Error(err));
                    return;
                }
            },
            Err(err) => {
                let _ = event_tx.send(SamplerEvent::Error(format!(
                    "failed reading top stream: {err}"
                )));
                return;
            }
        }
    }
}

fn wait_for_restart_or_stop(
    cmd_rx: &Receiver<CollectorCommand>,
    paused: &mut bool,
    sampler: &mut Option<SamplerRuntime>,
) -> bool {
    match cmd_rx.recv_timeout(SAMPLER_RESTART_BACKOFF) {
        Ok(cmd) => handle_collector_command(cmd, paused, sampler),
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => {
            if let Some(runtime) = sampler.take() {
                shutdown_sampler(runtime);
            }
            true
        }
    }
}

fn drain_collector_commands(
    cmd_rx: &Receiver<CollectorCommand>,
    paused: &mut bool,
    sampler: &mut Option<SamplerRuntime>,
) -> bool {
    loop {
        match cmd_rx.try_recv() {
            Ok(cmd) => {
                if handle_collector_command(cmd, paused, sampler) {
                    return true;
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                if let Some(runtime) = sampler.take() {
                    shutdown_sampler(runtime);
                }
                return true;
            }
        }
    }
}

fn handle_collector_command(
    cmd: CollectorCommand,
    paused: &mut bool,
    sampler: &mut Option<SamplerRuntime>,
) -> bool {
    match cmd {
        CollectorCommand::SetPaused(next) => {
            *paused = next;
            false
        }
        CollectorCommand::Stop => {
            if let Some(runtime) = sampler.take() {
                shutdown_sampler(runtime);
            }
            true
        }
    }
}

fn shutdown_sampler(mut sampler: SamplerRuntime) {
    match sampler.child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = sampler.child.kill();
            let _ = sampler.child.wait();
        }
        Err(_) => {
            let _ = sampler.child.kill();
            let _ = sampler.child.wait();
        }
    }

    let _ = sampler.reader.join();
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

fn history_range(values: &[f64]) -> Option<(f64, f64)> {
    let mut iter = values.iter().copied();
    let first = iter.next()?;

    let mut min = first;
    let mut max = first;

    for value in iter {
        min = min.min(value);
        max = max.max(value);
    }

    Some((min, max))
}

fn history_viewport_samples(values: &[f64], width: usize) -> Vec<f64> {
    let points = width.saturating_add(1);
    if points == 0 {
        return Vec::new();
    }

    if values.is_empty() {
        return vec![0.0; points];
    }

    if values.len() >= points {
        return values[values.len() - points..].to_vec();
    }

    let mut samples = vec![0.0; points];
    let start = points - values.len();
    samples[start..].copy_from_slice(values);
    samples
}

fn graph_scale_bounds(values: &[f64]) -> (f64, f64) {
    let (raw_min, raw_max) = history_range(values).unwrap_or((0.0, 0.0));

    if raw_max <= 0.0 {
        return (0.0, 1.0);
    }

    let span = (raw_max - raw_min).max(0.0);
    let target_span = (raw_max * 0.25).max(1.0);

    let base_max = if span < target_span {
        raw_max + (target_span - span) * 0.5
    } else {
        raw_max
    };

    let adjusted_max = (base_max * 1.35).max(raw_max + 5.0);

    (0.0, adjusted_max.max(1.0))
}

fn value_to_vertical_steps(value: f64, min: f64, max: f64, rows: usize) -> i32 {
    if rows == 0 || max <= min {
        return 0;
    }

    let max_steps = (rows * 4) as i32;
    let normalized = ((value - min) / (max - min)).clamp(0.0, 1.0);

    (normalized * max_steps as f64).round() as i32
}

fn spectrum_band_color(intensity: f64) -> Color {
    if intensity < 0.45 {
        COLOR_GREEN
    } else if intensity < 0.70 {
        COLOR_YELLOW
    } else if intensity < 0.90 {
        COLOR_ORANGE
    } else {
        COLOR_RED
    }
}

fn graph_span_style(color: Option<Color>) -> Style {
    match color {
        Some(color) => Style::default().fg(color).bg(COLOR_BG),
        None => Style::default().fg(COLOR_BG).bg(COLOR_BG),
    }
}

fn braille_history_cells(values: &[f64], width: usize, height: usize) -> Vec<Vec<(char, f64)>> {
    if height == 0 {
        return Vec::new();
    }

    if width == 0 {
        return vec![Vec::new(); height];
    }

    let samples = history_viewport_samples(values, width);
    let (scale_min, scale_max) = graph_scale_bounds(&samples);

    let steps: Vec<i32> = samples
        .iter()
        .map(|value| value_to_vertical_steps(*value, scale_min, scale_max, height))
        .collect();

    let max_steps = (height * 4).max(1) as i32;

    let mut rows = Vec::with_capacity(height);

    for row_from_top in 0..height {
        let row_from_bottom = height - 1 - row_from_top;
        let row_base = (row_from_bottom * 4) as i32;

        let mut line = Vec::with_capacity(width);
        for col in 0..width {
            let prev_level = (steps[col] - row_base).clamp(0, 4) as usize;
            let curr_level = (steps[col + 1] - row_base).clamp(0, 4) as usize;
            let peak_step = (row_base + prev_level.max(curr_level) as i32).clamp(0, max_steps);
            let intensity = (peak_step as f64 / max_steps as f64).clamp(0.0, 1.0);

            line.push((BRAILLE_5X5[prev_level * 5 + curr_level], intensity));
        }

        rows.push(line);
    }

    rows
}

fn braille_history_lines(values: &[f64], width: usize, height: usize) -> Vec<Line<'static>> {
    braille_history_cells(values, width, height)
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;

            for (ch, intensity) in row {
                let color = if ch == ' ' {
                    None
                } else {
                    Some(spectrum_band_color(intensity))
                };

                if color != run_color && !run.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        graph_span_style(run_color),
                    ));
                }

                run.push(ch);
                run_color = color;
            }

            if !run.is_empty() {
                spans.push(Span::styled(run, graph_span_style(run_color)));
            }

            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
fn braille_history_rows(values: &[f64], width: usize, height: usize) -> Vec<String> {
    braille_history_cells(values, width, height)
        .into_iter()
        .map(|row| row.into_iter().map(|(ch, _)| ch).collect())
        .collect()
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    let base_style = Style::default().bg(COLOR_BG).fg(COLOR_FG);
    frame.render_widget(Block::default().style(base_style), frame.area());

    app.rebuild_visible_if_needed();
    let visible_len = app.visible_indices.len();
    app.normalize_selection(visible_len);

    let pinned = app.pinned.clone();
    let pinned_row = pinned
        .as_ref()
        .and_then(|pin| {
            app.snapshot
                .rows
                .iter()
                .find(|row| row.pid == pin.pid && row.process == pin.process)
        })
        .cloned();

    let pinned_rank = pinned.as_ref().and_then(|pin| {
        app.visible_indices.iter().position(|idx| {
            app.snapshot.rows[*idx].pid == pin.pid && app.snapshot.rows[*idx].process == pin.process
        })
    });

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
    ];

    if let Some(pin) = pinned.as_ref() {
        top_spans.push(Span::styled(" • pinned:", Style::default().fg(COLOR_MUTED)));
        top_spans.push(Span::styled(
            pin.pid.to_string(),
            Style::default().fg(COLOR_ACCENT),
        ));
    }

    top_spans.push(Span::styled(" • ", Style::default().fg(COLOR_MUTED)));
    top_spans.push(Span::styled(
        app.status_hint(),
        Style::default().fg(COLOR_MUTED),
    ));

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

    let graph_block = panel_block().title("Power history (braille)");
    let graph_inner = graph_block.inner(layout[1]);
    frame.render_widget(graph_block, layout[1]);

    let graph_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(graph_inner);

    let (history_min, history_max) = history_range(&app.power_history).unwrap_or((0.0, 0.0));
    let graph_label = Paragraph::new(Line::from(vec![
        Span::styled("POWER ", Style::default().fg(COLOR_MUTED)),
        Span::styled(
            format!("{:.1}", app.snapshot.total_power),
            Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  • {} points  • range {:.1}–{:.1}",
                app.power_history.len(),
                history_min,
                history_max
            ),
            Style::default().fg(COLOR_MUTED),
        ),
    ]));
    frame.render_widget(graph_label, graph_rows[0]);

    let graph_lines = braille_history_lines(
        &app.power_history,
        graph_rows[1].width as usize,
        graph_rows[1].height as usize,
    );

    let graph = Paragraph::new(graph_lines)
        .style(Style::default().bg(COLOR_BG))
        .wrap(Wrap { trim: false });
    frame.render_widget(graph, graph_rows[1]);

    let table_region = layout[2];
    let (detail_area, rows_area) = if pinned.is_some() {
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
        let detail_lines = match (pinned.as_ref(), pinned_row.as_ref()) {
            (Some(_), Some(row)) => {
                let power_share = if app.snapshot.total_power > 0.0 {
                    (row.power_num / app.snapshot.total_power) * 100.0
                } else {
                    0.0
                };

                let rank_text = pinned_rank
                    .map(|rank| format!("#{} / {}", rank + 1, visible_len))
                    .unwrap_or_else(|| "not in current filtered list".to_string());

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
                        Span::styled(rank_text, Style::default().fg(COLOR_FG)),
                    ]),
                    Line::from(vec![
                        Span::styled("share ", Style::default().fg(COLOR_MUTED)),
                        Span::styled(
                            format!("{power_share:.1}% of total"),
                            Style::default().fg(COLOR_RED),
                        ),
                    ]),
                ]
            }
            (Some(pin), None) => vec![
                Line::from(Span::styled(
                    format!("{} ({})", pin.process, pin.pid),
                    Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Not present in the latest top sample.",
                    Style::default().fg(COLOR_MUTED),
                )),
                Line::from(Span::styled(
                    "Process may have exited or changed state.",
                    Style::default().fg(COLOR_MUTED),
                )),
            ],
            _ => vec![],
        };

        let detail_title = if let Some(pin) = pinned.as_ref() {
            format!("Pinned process {} • Enter unpin", pin.pid)
        } else {
            "Pinned process".to_string()
        };

        let detail = Paragraph::new(detail_lines)
            .block(panel_block().title(detail_title))
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
        let is_pinned_row = pinned
            .as_ref()
            .map(|pin| pin.pid == r.pid && pin.process == r.process)
            .unwrap_or(false);

        let row_style = if is_pinned_row {
            Style::default()
                .bg(COLOR_SELECTED_BG)
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_FG)
        };

        Row::new([
            Cell::from(r.pid.to_string()),
            Cell::from(r.process.clone()),
            Cell::from(r.power.clone()).style(Style::default().fg(COLOR_RED)),
        ])
        .style(row_style)
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

    let pin_suffix = pinned
        .as_ref()
        .map(|pin| format!(" • pinned pid {}", pin.pid))
        .unwrap_or_default();

    let table_title = if app.filter_input.is_some() {
        format!(
            "Processes {visible_len}/{} • filter edit: {}{}",
            app.snapshot.rows.len(),
            app.active_filter(),
            pin_suffix,
        )
    } else if app.filter_query.is_empty() {
        format!(
            "Processes {visible_len}/{} • power ↓{}",
            app.snapshot.rows.len(),
            pin_suffix,
        )
    } else {
        format!(
            "Processes {visible_len}/{} • power ↓ • filter: {}{}",
            app.snapshot.rows.len(),
            app.filter_query,
            pin_suffix,
        )
    };

    let highlight_style = if app.is_pinned() {
        Style::default().fg(COLOR_MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::default()
            .bg(COLOR_SELECTED_BG)
            .fg(COLOR_FG)
            .add_modifier(Modifier::BOLD)
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
    .row_highlight_style(highlight_style);

    let selected_in_window = if visible_len == 0 || app.is_pinned() {
        None
    } else {
        Some(app.selected.saturating_sub(start))
    };
    let mut table_state = TableState::default().with_selected(selected_in_window);
    frame.render_stateful_widget(table, rows_area, &mut table_state);
}

fn snapshot_from_rows(rows: Vec<ProcRow>) -> Snapshot {
    let total_power = rows.iter().map(|r| r.power_num).sum::<f64>();
    Snapshot { rows, total_power }
}

fn fetch_snapshot() -> io::Result<Snapshot> {
    let child = Command::new(TOP_BIN)
        .args(TOP_ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut excluded_pids = Vec::with_capacity(2);
    if let Ok(top_pid) = i32::try_from(child.id()) {
        excluded_pids.push(top_pid);
    }
    if let Ok(self_pid) = i32::try_from(std::process::id()) {
        excluded_pids.push(self_pid);
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "top command failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows = parse_second_sample_excluding(&stdout, &excluded_pids);
    Ok(snapshot_from_rows(rows))
}

fn parse_second_sample_excluding(raw: &str, excluded_pids: &[i32]) -> Vec<ProcRow> {
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
            if excluded_pids.contains(&row.pid) {
                continue;
            }
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

        let rows = parse_second_sample_excluding(raw, &[]);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![99, 100]);
        assert_eq!(rows[1].process, "Google Chrome Helper");
    }

    #[test]
    fn parse_second_sample_excludes_internal_pids() {
        let raw = r#"
PID COMMAND POWER
10 etop 5.0
20 top 3.0
30 Safari 1.0
"#;

        let rows = parse_second_sample_excluding(raw, &[10, 20]);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![30]);
    }

    #[test]
    fn top_stream_parser_skips_warmup_frame() {
        let mut parser = TopStreamParser::new(vec![]);
        let mut snapshots = Vec::new();

        let lines = [
            "PID COMMAND POWER",
            "10 Safari 1.0",
            "",
            "PID COMMAND POWER",
            "20 Finder 2.5",
            "",
        ];

        for line in lines {
            if let Some(snapshot) = parser.push_line(line).expect("stream line should parse") {
                snapshots.push(snapshot);
            }
        }

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].rows.len(), 1);
        assert_eq!(snapshots[0].rows[0].pid, 20);
        assert!((snapshots[0].total_power - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn top_stream_parser_excludes_internal_pids() {
        let mut parser = TopStreamParser::new(vec![30]);

        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");
        parser
            .push_line("10 warmup 0.1")
            .expect("warmup row should parse");
        parser.push_line("").expect("warmup end should parse");

        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");
        parser
            .push_line("30 top 7.0")
            .expect("excluded row should parse");
        let snapshot = parser
            .push_line("31 Safari 1.0")
            .expect("row should parse")
            .or_else(|| parser.push_line("").expect("frame boundary should parse"))
            .expect("second frame should emit");

        let pids: Vec<i32> = snapshot.rows.iter().map(|row| row.pid).collect();
        assert_eq!(pids, vec![31]);
    }

    #[test]
    fn top_stream_parser_reports_row_parse_errors() {
        let mut parser = TopStreamParser::new(vec![]);
        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");

        let err = parser
            .push_line("123")
            .expect_err("malformed row should fail parsing");
        assert!(err.contains("unable to parse top row"));
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

    #[test]
    fn history_viewport_samples_keeps_latest_points_without_resampling() {
        let samples = history_viewport_samples(&[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        assert_eq!(samples, vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn history_viewport_samples_shifts_left_as_new_samples_arrive() {
        let width = 3;
        let before = history_viewport_samples(&[0.0, 1.0, 2.0, 3.0], width);
        let after = history_viewport_samples(&[0.0, 1.0, 2.0, 3.0, 4.0], width);

        assert_eq!(&after[..width], &before[1..]);
        assert_eq!(after[width], 4.0);
    }

    #[test]
    fn history_viewport_samples_right_aligns_short_history() {
        let samples = history_viewport_samples(&[7.0, 8.0], 4);
        assert_eq!(samples, vec![0.0, 0.0, 0.0, 7.0, 8.0]);
    }

    #[test]
    fn braille_history_rows_single_row_uses_lookup_mapping() {
        let rows = braille_history_rows(&[0.0, 10.0], 1, 1);
        assert_eq!(rows, vec!["⢰".to_string()]);
    }

    #[test]
    fn braille_history_rows_respects_requested_dimensions() {
        let rows = braille_history_rows(&[0.0, 5.0, 10.0], 6, 3);

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.chars().count() == 6));
    }

    #[test]
    fn braille_history_rows_defaults_to_blanks_when_empty() {
        let rows = braille_history_rows(&[], 4, 2);
        assert_eq!(rows, vec!["    ".to_string(), "    ".to_string()]);
    }

    #[test]
    fn spectrum_band_color_uses_expected_breakpoints() {
        assert_eq!(spectrum_band_color(0.10), COLOR_GREEN);
        assert_eq!(spectrum_band_color(0.50), COLOR_YELLOW);
        assert_eq!(spectrum_band_color(0.75), COLOR_ORANGE);
        assert_eq!(spectrum_band_color(0.95), COLOR_RED);
    }

    #[test]
    fn braille_history_lines_color_high_values_use_orange_with_extra_headroom() {
        let lines = braille_history_lines(&[0.0, 10.0], 1, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(COLOR_ORANGE));
    }

    #[test]
    fn graph_scale_bounds_adds_headroom_for_flat_history() {
        let (_, max) = graph_scale_bounds(&[5.0, 5.0, 5.0]);
        assert!(max > 5.0);
    }

    #[test]
    fn graph_scale_bounds_adds_headroom_for_active_history() {
        let (_, max) = graph_scale_bounds(&[39.3, 64.1, 118.8]);
        assert!(max > 118.8);
    }
}
