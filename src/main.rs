use std::{
    cmp::Ordering,
    collections::VecDeque,
    io::{self, BufRead, BufReader},
    process::{Child, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

mod app_state;
mod archive_query;
mod graph;
mod history;
mod persistence;
mod top_parse;

use app_state::{App, GraphHeatSettings, SETTINGS_FIELDS, SettingsModalState};
use crossterm::event::{self, Event};
#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent};
#[cfg(test)]
use graph::GraphRange;
use graph::{
    braille_history_lines_optional_with_scale, braille_history_lines_with_scale,
    graph_scale_bounds, graph_scale_bounds_optional, history_viewport_samples,
    history_viewport_samples_deque,
};
use history::PidKey;
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
#[cfg(test)]
use top_parse::{ProcRow, snapshot_from_rows};
use top_parse::{Snapshot, TopStreamParser, fetch_snapshot};

const TOP_BIN: &str = "top";
const REFRESH_EVERY: Duration = Duration::from_secs(2);
const REDRAW_EVERY: Duration = Duration::from_millis(120);
const COLLECTOR_POLL_EVERY: Duration = Duration::from_millis(200);
const SAMPLER_RESTART_BACKOFF: Duration = Duration::from_secs(1);
const SAMPLER_QUEUE_CAPACITY: usize = 8;
const HISTORY_LIMIT: usize = 240;
const HISTORY_STALE_TICKS: u64 = HISTORY_LIMIT as u64;
const PROCESS_AVG_WINDOW_TICKS: u64 = 60;
const PROCESS_PEAK_WINDOW_TICKS: u64 = 60;
const PERSIST_FLUSH_EVERY_TICKS: u64 = 15;

const COLOR_FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
const COLOR_ACCENT: Color = Color::Rgb(0x61, 0xaf, 0xef);
const COLOR_MUTED: Color = Color::Rgb(0x5c, 0x63, 0x70);
const COLOR_SELECTED_BG: Color = Color::Rgb(0x2c, 0x31, 0x3c);
const COLOR_GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
const COLOR_YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const COLOR_ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
const COLOR_RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);

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

    match persistence::load_session_cache_for_startup(REFRESH_EVERY, HISTORY_LIMIT) {
        Ok(Some(loaded)) => app.apply_loaded_session_cache(loaded),
        Ok(None) => {}
        Err(err) => {
            app.last_error = Some(format!("cache load failed: {err}"));
        }
    }

    let mut last_persist_tick = app.tick;

    let (event_tx, event_rx) = mpsc::channel::<CollectorEvent>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<CollectorCommand>();

    let collector = thread::spawn(move || collector_loop(event_tx, cmd_rx));

    let loop_result = (|| -> io::Result<()> {
        loop {
            drain_collector_events(&mut app, &event_rx);

            if app.tick.wrapping_sub(last_persist_tick) >= PERSIST_FLUSH_EVERY_TICKS {
                if let Err(err) = persistence::save_session_cache(&app.to_session_cache()) {
                    app.last_error = Some(format!("cache flush failed: {err}"));
                }
                last_persist_tick = app.tick;
            }

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

    if let Err(err) = persistence::save_session_cache(&app.to_session_cache()) {
        eprintln!("etop: failed to flush cache on shutdown: {err}");
    }

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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_MUTED))
        .title_style(
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().fg(COLOR_FG))
}

fn chip_line(label: &str, hotkey: Option<char>) -> Vec<Span<'static>> {
    let label_style = Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD);

    let Some(hotkey) = hotkey else {
        return vec![Span::styled(label.to_string(), label_style)];
    };

    let lower = hotkey.to_ascii_lowercase();
    let upper = hotkey.to_ascii_uppercase();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut highlighted = false;

    for ch in label.chars() {
        if !highlighted && (ch == lower || ch == upper) {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), label_style));
            }
            spans.push(Span::styled(ch.to_string(), key_style));
            highlighted = true;
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, label_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(label.to_string(), label_style));
    }
    spans
}

fn action_chip_line(label: &str, hotkey: &str) -> Vec<Span<'static>> {
    if hotkey.chars().count() == 1 {
        return chip_line(label, hotkey.chars().next());
    }

    let label_style = Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD);

    vec![
        Span::styled(hotkey.to_string(), key_style),
        Span::styled(format!(" {label}"), label_style),
    ]
}

fn draw_chips_on_border_right(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    end_x: u16,
    chips: &[Vec<Span<'_>>],
) {
    if chips.is_empty() || area.width == 0 {
        return;
    }
    let total_width: u16 = chips
        .iter()
        .map(|chip| {
            let inner: u16 = chip
                .iter()
                .map(|span| span.content.chars().count() as u16)
                .sum();
            inner + 2
        })
        .sum();
    let left_bound = area.x + 1;
    if end_x <= left_bound + total_width {
        return;
    }
    let start_x = end_x.saturating_sub(total_width);
    draw_chips_on_border(buf, area, y, start_x, chips);
}

fn draw_chips_on_border(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    start_x: u16,
    chips: &[Vec<Span<'_>>],
) -> u16 {
    let cap_style = Style::default().fg(COLOR_MUTED);
    let is_bottom = y + 1 == area.y + area.height;
    let (left_cap, right_cap) = if is_bottom {
        ("┘", "└")
    } else {
        ("┐", "┌")
    };

    if y < area.y || y >= area.y + area.height {
        return start_x;
    }

    let mut x = start_x;
    let right_edge = area.x + area.width.saturating_sub(1);

    for chip in chips {
        let inner_width: u16 = chip
            .iter()
            .map(|span| span.content.chars().count() as u16)
            .sum();
        let chip_width = inner_width + 2; // caps on each side
        if x + chip_width > right_edge {
            break;
        }

        buf[(x, y)].set_symbol(left_cap).set_style(cap_style);
        x += 1;

        for span in chip {
            for ch in span.content.chars() {
                if x >= right_edge {
                    break;
                }
                let mut tmp = [0u8; 4];
                let s = ch.encode_utf8(&mut tmp);
                buf[(x, y)].set_symbol(s).set_style(span.style);
                x += 1;
            }
        }

        buf[(x, y)].set_symbol(right_cap).set_style(cap_style);
        x += 1;
    }

    x
}

fn hotkey_hint_line(hint: &str) -> Line<'static> {
    let key_style = Style::default()
        .fg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(COLOR_MUTED);
    let sep_style = Style::default().fg(COLOR_MUTED);

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (idx, chunk) in hint.split(" • ").enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" • ", sep_style));
        }
        match chunk.split_once(' ') {
            Some((key, rest)) if !key.is_empty() && !rest.is_empty() && !key.ends_with(':') => {
                spans.push(Span::styled(key.to_string(), key_style));
                spans.push(Span::styled(format!(" {rest}"), text_style));
            }
            _ => spans.push(Span::styled(chunk.to_string(), text_style)),
        }
    }
    Line::from(spans)
}

fn spectrum_band_color(power: f64, thresholds: &GraphHeatSettings) -> Color {
    thresholds.color_for_power(power)
}

const PIN_MARKER: &str = "▍";

fn pin_marker_cell(is_pinned: bool) -> Cell<'static> {
    if is_pinned {
        Cell::from(Span::styled(
            PIN_MARKER,
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Cell::from(" ")
    }
}

fn sort_header_cell(label: &'static str, active: bool, _width: u16) -> Cell<'static> {
    let label_style = Style::default()
        .fg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD);
    let arrow_style = Style::default()
        .fg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD);

    let (arrow, a_style) = if active {
        ("↓", arrow_style)
    } else {
        (" ", Style::default())
    };
    Cell::from(Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(arrow, a_style),
    ]))
}

#[derive(Clone, Copy, Debug)]
struct TableLayout {
    show_pid: bool,
    show_avg: bool,
    show_peak: bool,
    trend_width: u16,
}

fn compute_process_col_width(panel_width: u16) -> u16 {
    let inner = panel_width.saturating_sub(4);
    if inner < 40 {
        20
    } else if inner < 80 {
        24
    } else if inner < 120 {
        28
    } else {
        32
    }
}

impl TableLayout {
    fn for_processes(width: u16) -> Self {
        Self {
            show_pid: width >= 74,
            show_avg: width >= 58,
            show_peak: width >= 70,
            trend_width: trend_column_width(width),
        }
    }
}

fn trend_column_width(panel_width: u16) -> u16 {
    if panel_width >= 140 {
        24
    } else if panel_width >= 110 {
        18
    } else if panel_width >= 90 {
        14
    } else {
        0
    }
}

fn trend_sparkline_cell(samples: &[f64], width: u16) -> Cell<'static> {
    let w = width as usize;
    if w == 0 || samples.is_empty() {
        return Cell::from("");
    }
    let (scale_min, scale_max) = graph_scale_bounds(samples);
    let mut lines = braille_history_lines_with_scale(samples, w, 1, scale_min, scale_max);
    if lines.is_empty() {
        return Cell::from("");
    }
    Cell::from(lines.remove(0))
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    app.rebuild_process_rows_if_needed();

    let process_visible_len = app.process_rows.len();
    app.normalize_process_selection(process_visible_len);

    let pinned = app.pinned.clone();
    let pinned_live_row = pinned
        .as_ref()
        .and_then(|pin| {
            app.snapshot
                .rows
                .iter()
                .find(|row| row.pid == pin.pid && row.process == pin.process)
        })
        .cloned();

    let pinned_visible_rank = pinned.as_ref().and_then(|pin| {
        app.process_rows
            .iter()
            .position(|row| row.key.pid == pin.pid && row.key.process == pin.process)
    });
    let controls_blocked = app.settings_modal.is_some() || app.is_filter_input_active();

    if !app.show_graph && !app.show_table {
        draw_easter_egg(
            frame,
            frame.area(),
            app.tick,
            &app.power_history,
            app.snapshot.total_power,
        );
        if let Some(modal) = app.settings_modal.as_ref() {
            draw_settings_modal(frame, modal);
        }
        return;
    }

    let constraints: Vec<Constraint> = match (app.show_graph, app.show_table) {
        (true, true) => vec![Constraint::Percentage(34), Constraint::Min(8)],
        (true, false) => vec![Constraint::Min(1)],
        (false, true) => vec![Constraint::Min(1)],
        (false, false) => unreachable!(),
    };
    let layout = Layout::vertical(constraints).split(frame.area());
    let (graph_slot, table_slot) = match (app.show_graph, app.show_table) {
        (true, true) => (Some(layout[0]), Some(layout[1])),
        (true, false) => (Some(layout[0]), None),
        (false, true) => (None, Some(layout[0])),
        (false, false) => unreachable!(),
    };

    let paused_chip_style = Style::default()
        .fg(COLOR_YELLOW)
        .add_modifier(Modifier::BOLD);
    let loading_chip_style = Style::default()
        .fg(COLOR_YELLOW)
        .add_modifier(Modifier::BOLD);

    if let Some(graph_area) = graph_slot
        && graph_area.height >= 3
        && graph_area.width >= 3
    {
        let graph_inner = panel_block().inner(graph_area);
        let graph_width = graph_inner.width as usize;
        let graph_height = graph_inner.height as usize;

        let graph_samples = app.main_graph_live_samples_for_width(graph_width);
        let (scale_min, scale_max) = app.graph_scale_bounds_for_viewport(&graph_samples);
        let graph_lines = braille_history_lines_with_scale(
            &graph_samples,
            graph_width,
            graph_height,
            scale_min,
            scale_max,
        );
        let mut graph_block = panel_block();
        if let Some(error) = app.last_error.as_deref() {
            graph_block = graph_block.title_bottom(Line::from(Span::styled(
                format!("error: {error}"),
                Style::default().fg(COLOR_RED),
            )));
        }
        frame.render_widget(graph_block, graph_area);

        let mut graph_chips: Vec<Vec<Span<'static>>> = vec![chip_line("¹etop", Some('¹'))];
        if app.paused {
            let mut spans = chip_line("PAUSED", None);
            for span in &mut spans {
                span.style = span.style.patch(paused_chip_style);
            }
            graph_chips.push(spans);
        }
        if app.loading {
            let mut spans = chip_line("LOADING", None);
            for span in &mut spans {
                span.style = span.style.patch(loading_chip_style);
            }
            graph_chips.push(spans);
        }
        let border_y = graph_area.y;
        draw_chips_on_border(
            frame.buffer_mut(),
            graph_area,
            border_y,
            graph_area.x + 1,
            &graph_chips,
        );

        let power_chip: Vec<Span<'static>> = vec![
            Span::styled("power ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                format!("{:.1}", app.snapshot.total_power),
                Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
            ),
        ];
        let scale_chip: Vec<Span<'static>> = vec![Span::styled(
            format!("{:.0}–{:.0}", scale_min, scale_max),
            Style::default().fg(COLOR_MUTED),
        )];
        let right_chips = vec![power_chip, scale_chip];
        let right_edge = graph_area.x + graph_area.width.saturating_sub(1);
        draw_chips_on_border_right(
            frame.buffer_mut(),
            graph_area,
            border_y,
            right_edge,
            &right_chips,
        );

        let bottom_y = graph_area.y + graph_area.height.saturating_sub(1);
        if !controls_blocked {
            if !app.show_table {
                let graph_bottom_left_chips =
                    vec![action_chip_line("menu", "m"), action_chip_line("quit", "q")];
                draw_chips_on_border(
                    frame.buffer_mut(),
                    graph_area,
                    bottom_y,
                    graph_area.x + 1,
                    &graph_bottom_left_chips,
                );
            }

            let graph_bottom_right_chips = vec![action_chip_line("pause", "p")];
            let right_edge = graph_area.x + graph_area.width.saturating_sub(1);
            draw_chips_on_border_right(
                frame.buffer_mut(),
                graph_area,
                bottom_y,
                right_edge,
                &graph_bottom_right_chips,
            );
        }

        let graph = Paragraph::new(graph_lines);
        frame.render_widget(graph, graph_inner);
    }

    let Some(table_region) = table_slot.filter(|r| r.height >= 3 && r.width >= 3) else {
        if let Some(modal) = app.settings_modal.as_ref() {
            draw_settings_modal(frame, modal);
        }
        return;
    };
    let show_detail = pinned.is_some();

    let (detail_area, rows_area) = if show_detail {
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
        let detail_title = if let Some(pin) = pinned.as_ref() {
            format!("pinned {}", pin.pid)
        } else {
            "pinned process".to_string()
        };

        let archive_backed_mode = app.graph_range.archive_range().is_some();
        let detail_range_label = if archive_backed_mode {
            format!("hist {}", app.graph_range.label())
        } else {
            app.graph_range.label().to_string()
        };
        let detail_block =
            panel_block().title_top(format!("{detail_title} • {detail_range_label}"));
        let detail_inner = detail_block.inner(detail_rect);
        frame.render_widget(detail_block, detail_rect);
        if !controls_blocked {
            let detail_bottom_y = detail_rect.y + detail_rect.height.saturating_sub(1);
            let detail_right_edge = detail_rect.x + detail_rect.width.saturating_sub(1);
            let detail_bottom_chips = vec![
                action_chip_line("range", "r"),
                action_chip_line("unpin", "Enter"),
            ];
            draw_chips_on_border_right(
                frame.buffer_mut(),
                detail_rect,
                detail_bottom_y,
                detail_right_edge,
                &detail_bottom_chips,
            );
        }

        let pinned_archive_samples = pinned.as_ref().and_then(|pin| {
            if detail_inner.width == 0 {
                None
            } else {
                let key = PidKey::new(pin.pid, pin.process.clone());
                app.pid_archive_samples_for_width(&key, detail_inner.width as usize)
            }
        });

        let text_lines: Vec<Line> = match pinned.as_ref() {
            Some(pin) => {
                let key = PidKey::new(pin.pid, pin.process.clone());
                let current = pinned_live_row
                    .as_ref()
                    .map(|row| row.power_num)
                    .unwrap_or(0.0);
                let avg = app
                    .history_store
                    .pid_avg(&key, PROCESS_AVG_WINDOW_TICKS, app.tick);
                let peak = app
                    .history_store
                    .pid_peak(&key, PROCESS_PEAK_WINDOW_TICKS, app.tick);
                let rank_text = pinned_visible_rank
                    .map(|rank| format!("#{} / {}", rank + 1, process_visible_len))
                    .unwrap_or_else(|| "not in current filtered list".to_string());

                let mut lines = vec![Line::from(Span::styled(
                    format!("{} ({})", pin.process, pin.pid),
                    Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
                ))];

                lines.push(Line::from(vec![
                    Span::styled("now ", Style::default().fg(COLOR_MUTED)),
                    Span::styled(
                        format!("{current:.1}"),
                        Style::default()
                            .fg(spectrum_band_color(current, &app.settings.graph_heat))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  avg2m ", Style::default().fg(COLOR_MUTED)),
                    Span::styled(format!("{avg:.1}"), Style::default().fg(COLOR_FG)),
                    Span::styled("  peak2m ", Style::default().fg(COLOR_MUTED)),
                    Span::styled(format!("{peak:.1}"), Style::default().fg(COLOR_FG)),
                    Span::styled("  live rank ", Style::default().fg(COLOR_MUTED)),
                    Span::styled(rank_text, Style::default().fg(COLOR_FG)),
                ]));

                if archive_backed_mode {
                    match pinned_archive_samples
                        .as_deref()
                        .and_then(archive_bucket_metrics)
                    {
                        Some(metrics) => lines.push(Line::from(vec![
                            Span::styled(
                                format!("hist {} pid buckets ", app.graph_range.label()),
                                Style::default().fg(COLOR_MUTED),
                            ),
                            Span::styled("avg bucket ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                format!("{:.1}", metrics.avg_bucket_power),
                                Style::default().fg(COLOR_FG),
                            ),
                            Span::styled("  max bucket avg ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                format!("{:.1}", metrics.max_bucket_power),
                                Style::default().fg(COLOR_FG),
                            ),
                            Span::styled("  buckets ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                format!("{}", metrics.bucket_count),
                                Style::default().fg(COLOR_FG),
                            ),
                        ])),
                        None => lines.push(Line::from(Span::styled(
                            format!(
                                "hist {} pid buckets: no historical data in selected range.",
                                app.graph_range.label()
                            ),
                            Style::default().fg(COLOR_MUTED),
                        ))),
                    }
                }

                if pinned_live_row.is_none() {
                    lines.push(Line::from(Span::styled(
                        "Not present in the latest top sample.",
                        Style::default().fg(COLOR_MUTED),
                    )));
                }

                lines
            }
            None => vec![],
        };

        let text_height = text_lines.len() as u16;
        let text_rect = Rect {
            x: detail_inner.x,
            y: detail_inner.y,
            width: detail_inner.width,
            height: text_height.min(detail_inner.height),
        };
        frame.render_widget(Paragraph::new(text_lines), text_rect);

        let mini_graph_rect = if detail_inner.height > text_height {
            Some(Rect {
                x: detail_inner.x,
                y: detail_inner.y + text_height,
                width: detail_inner.width,
                height: detail_inner.height - text_height,
            })
        } else {
            None
        };

        if let (Some(graph_rect), Some(pin)) = (mini_graph_rect, pinned.as_ref()) {
            let graph_w = graph_rect.width as usize;
            let graph_h = graph_rect.height as usize;
            if graph_w > 0 && graph_h > 0 {
                let key = PidKey::new(pin.pid, pin.process.clone());
                if archive_backed_mode {
                    if let Some(archive_samples) = pinned_archive_samples.as_ref() {
                        let (mini_min, mini_max) = graph_scale_bounds_optional(archive_samples);
                        let mini_lines = braille_history_lines_optional_with_scale(
                            archive_samples,
                            graph_w,
                            graph_h,
                            mini_min,
                            mini_max,
                        );
                        frame.render_widget(Paragraph::new(mini_lines), graph_rect);
                    }
                } else {
                    let history_samples =
                        app.history_store
                            .pid_recent_values(&key, HISTORY_LIMIT as u64, app.tick);
                    let samples = history_viewport_samples(&history_samples, graph_w);
                    let (mini_min, mini_max) = graph_scale_bounds(&samples);
                    let mini_lines = braille_history_lines_with_scale(
                        &history_samples,
                        graph_w,
                        graph_h,
                        mini_min,
                        mini_max,
                    );
                    frame.render_widget(Paragraph::new(mini_lines), graph_rect);
                }
            }
        }
    }

    let rows_visible = rows_area.height.saturating_sub(3) as usize;

    if rows_visible > 0 && process_visible_len > 0 {
        if app.process_selected < app.process_scroll {
            app.process_scroll = app.process_selected;
        }
        if app.process_selected >= app.process_scroll + rows_visible {
            app.process_scroll = app.process_selected + 1 - rows_visible;
        }
    } else {
        app.process_scroll = 0;
    }

    let start = app.process_scroll.min(process_visible_len);
    let end = if rows_visible == 0 {
        start
    } else {
        (start + rows_visible).min(process_visible_len)
    };

    let layout = TableLayout::for_processes(rows_area.width);
    let graph_heat = app.settings.graph_heat.clone();
    let trend_width = layout.trend_width;
    let current_tick = app.tick;
    let sort = app.process_sort;
    let rows = app.process_rows[start..end]
        .iter()
        .map(|row| {
            let is_pinned_row = pinned
                .as_ref()
                .map(|pin| pin.pid == row.key.pid && pin.process == row.key.process)
                .unwrap_or(false);

            let mut cells: Vec<Cell<'static>> = Vec::with_capacity(7);
            cells.push(pin_marker_cell(is_pinned_row));
            if layout.show_pid {
                cells.push(Cell::from(row.key.pid.to_string()));
            }
            cells.push(Cell::from(row.key.process.clone()));
            cells.push(
                Cell::from(format!("{:.1}", row.current))
                    .style(Style::default().fg(spectrum_band_color(row.current, &graph_heat))),
            );
            if layout.show_avg {
                cells.push(Cell::from(format!("{:.1}", row.avg)));
            }
            if layout.show_peak {
                cells.push(Cell::from(format!("{:.1}", row.peak)));
            }
            if trend_width > 0 {
                let samples = app.history_store.pid_recent_values(
                    &row.key,
                    trend_width as u64 * 2,
                    current_tick,
                );
                cells.push(trend_sparkline_cell(&samples, trend_width));
            }
            Row::new(cells)
        })
        .collect::<Vec<_>>();

    let now_width: u16 = 8;
    let avg_width: u16 = 8;
    let peak_width: u16 = 8;

    let mut header_cells: Vec<Cell<'static>> = Vec::with_capacity(7);
    header_cells.push(Cell::from(" "));
    if layout.show_pid {
        header_cells.push(Cell::from("PID"));
    }
    header_cells.push(Cell::from(Span::styled(
        "PROCESS",
        Style::default()
            .fg(COLOR_ACCENT)
            .add_modifier(Modifier::BOLD),
    )));
    header_cells.push(sort_header_cell(
        "NOW",
        sort == app_state::ProcessSort::Current,
        now_width,
    ));
    if layout.show_avg {
        header_cells.push(sort_header_cell(
            "AVG2M",
            sort == app_state::ProcessSort::Avg2m,
            avg_width,
        ));
    }
    if layout.show_peak {
        header_cells.push(sort_header_cell(
            "PEAK",
            sort == app_state::ProcessSort::Peak,
            peak_width,
        ));
    }
    if layout.trend_width > 0 {
        header_cells.push(Cell::from(Span::styled(
            "TREND",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        )));
    }
    let header_row = Row::new(header_cells);

    let table_title_right = if app.process_filter_input.is_some() {
        format!(
            "{process_visible_len}/{} • filter edit: {}",
            app.snapshot.rows.len(),
            app.process_active_filter(),
        )
    } else if app.process_filter_query.is_empty() {
        format!("{process_visible_len}/{}", app.snapshot.rows.len())
    } else {
        format!(
            "{process_visible_len}/{} • filter: {}",
            app.snapshot.rows.len(),
            app.process_filter_query,
        )
    };

    let highlight_style = Style::default()
        .bg(COLOR_SELECTED_BG)
        .add_modifier(Modifier::BOLD);

    let table_block = panel_block().title_top(
        Line::from(Span::styled(
            table_title_right,
            Style::default().fg(COLOR_MUTED),
        ))
        .right_aligned(),
    );

    let process_col_width = compute_process_col_width(rows_area.width);
    let mut constraints: Vec<Constraint> = Vec::with_capacity(7);
    constraints.push(Constraint::Length(1));
    if layout.show_pid {
        constraints.push(Constraint::Length(7));
    }
    constraints.push(Constraint::Min(process_col_width));
    constraints.push(Constraint::Length(now_width));
    if layout.show_avg {
        constraints.push(Constraint::Length(avg_width));
    }
    if layout.show_peak {
        constraints.push(Constraint::Length(peak_width));
    }
    if layout.trend_width > 0 {
        constraints.push(Constraint::Length(layout.trend_width));
    }

    let table = Table::new(rows, constraints)
        .header(header_row)
        .block(table_block)
        .column_spacing(1)
        .style(Style::default().fg(COLOR_FG))
        .row_highlight_style(highlight_style);

    let selected_in_window = if process_visible_len == 0 || app.is_pinned() {
        None
    } else {
        Some(app.process_selected.saturating_sub(start))
    };
    let mut table_state = TableState::default().with_selected(selected_in_window);
    frame.render_stateful_widget(table, rows_area, &mut table_state);

    let table_chips = vec![chip_line("²processes", Some('²'))];
    draw_chips_on_border(
        frame.buffer_mut(),
        rows_area,
        rows_area.y,
        rows_area.x + 1,
        &table_chips,
    );

    if !controls_blocked {
        let bottom_y = rows_area.y + rows_area.height.saturating_sub(1);
        let bottom_left_chips = vec![
            action_chip_line("menu", "m"),
            action_chip_line("filter", "f"),
            action_chip_line("quit", "q"),
        ];
        draw_chips_on_border(
            frame.buffer_mut(),
            rows_area,
            bottom_y,
            rows_area.x + 1,
            &bottom_left_chips,
        );

        let bottom_right_chips = vec![action_chip_line("sort", "s")];
        let right_edge = rows_area.x + rows_area.width.saturating_sub(1);
        draw_chips_on_border_right(
            frame.buffer_mut(),
            rows_area,
            bottom_y,
            right_edge,
            &bottom_right_chips,
        );
    }

    if let Some(modal) = app.settings_modal.as_ref() {
        draw_settings_modal(frame, modal);
    }
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height_percent) / 2),
        Constraint::Percentage(height_percent),
        Constraint::Percentage((100 - height_percent) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .split(vertical[1])[1]
}

fn draw_easter_egg(
    frame: &mut Frame,
    area: Rect,
    tick: u64,
    power_history: &VecDeque<f64>,
    current_power: f64,
) {
    let block =
        panel_block().title_bottom(hotkey_hint_line("1 graph • 2 table • q quit").right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chips = vec![chip_line("etop", None)];
    draw_chips_on_border(frame.buffer_mut(), area, area.y, area.x + 1, &chips);

    let spark_height: u16 = 5;
    let stack_height: u16 = 1 /* power value */ + 1 /* spacer */ + spark_height + 1 /* spacer */ + 1 /* hint */;
    if inner.width < 12 || inner.height < stack_height {
        return;
    }

    let spark_width = (inner.width * 2 / 3).max(20).min(inner.width);
    let start_x = inner.x + (inner.width.saturating_sub(spark_width)) / 2;
    let start_y = inner.y + (inner.height - stack_height) / 2;

    let power_style = Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD);
    let unit_style = Style::default().fg(COLOR_MUTED);
    let hint_style = Style::default().fg(COLOR_MUTED);

    let power_line = Line::from(vec![
        Span::styled(format!("{current_power:.1}"), power_style),
        Span::styled(" W", unit_style),
    ])
    .centered();

    let value_rect = Rect {
        x: inner.x,
        y: start_y,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(power_line), value_rect);

    let spark_rect = Rect {
        x: start_x,
        y: start_y + 2,
        width: spark_width,
        height: spark_height,
    };
    let samples = history_viewport_samples_deque(power_history, spark_width as usize);
    let (scale_min, scale_max) = graph_scale_bounds(&samples);
    let spark_lines = braille_history_lines_with_scale(
        &samples,
        spark_width as usize,
        spark_height as usize,
        scale_min,
        scale_max,
    );
    frame.render_widget(Paragraph::new(spark_lines), spark_rect);

    let hint = if tick % 8 < 4 {
        "press 1 or 2 to bring a panel back"
    } else {
        "etop is still watching"
    };
    let hint_rect = Rect {
        x: inner.x,
        y: start_y + 2 + spark_height + 1,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, hint_style)).centered()),
        hint_rect,
    );
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ArchiveBucketMetrics {
    avg_bucket_power: f64,
    max_bucket_power: f64,
    bucket_count: usize,
}

fn archive_bucket_metrics(samples: &[Option<f64>]) -> Option<ArchiveBucketMetrics> {
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut max = f64::NEG_INFINITY;

    for value in samples.iter().flatten().copied() {
        sum += value;
        count += 1;
        max = max.max(value);
    }

    if count == 0 {
        return None;
    }

    Some(ArchiveBucketMetrics {
        avg_bucket_power: sum / count as f64,
        max_bucket_power: max,
        bucket_count: count,
    })
}

fn draw_settings_modal(frame: &mut Frame, modal: &SettingsModalState) {
    let area = centered_rect(60, 50, frame.area());
    frame.render_widget(Clear, area);

    let block =
        panel_block().title_bottom(hotkey_hint_line("m apply • Esc cancel").right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let menu_chip = vec![chip_line("menu", Some('m'))];
    draw_chips_on_border(frame.buffer_mut(), area, area.y, area.x + 1, &menu_chip);

    let mut lines = vec![
        Line::from(Span::styled(
            "Graph heat thresholds (absolute power)",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("", Style::default().fg(COLOR_MUTED))),
    ];

    for (idx, field) in SETTINGS_FIELDS.iter().enumerate() {
        let is_selected = idx == modal.selected;
        let is_editing = modal
            .editing
            .as_ref()
            .map(|edit| edit.field == *field)
            .unwrap_or(false);

        let value = if is_editing {
            format!("{}▏", modal.display_value(*field))
        } else {
            modal.display_value(*field)
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<14}", field.label()),
                if is_selected {
                    Style::default()
                        .fg(COLOR_ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_FG)
                },
            ),
            Span::styled(" ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                value,
                if is_editing {
                    Style::default()
                        .fg(COLOR_YELLOW)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_FG)
                },
            ),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "",
        Style::default().fg(COLOR_MUTED),
    )));

    if let Some(error) = modal.error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(COLOR_RED),
        )));
    } else if modal.editing.is_some() {
        lines.push(Line::from(Span::styled(
            "Enter confirm value • Esc cancel field edit",
            Style::default().fg(COLOR_MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "↑/↓ move • Enter edit field • m apply and close",
            Style::default().fg(COLOR_MUTED),
        )));
    }

    let content = Paragraph::new(lines)
        .style(Style::default().fg(COLOR_FG))
        .wrap(Wrap { trim: false });
    frame.render_widget(content, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, process: &str, power_num: f64) -> ProcRow {
        ProcRow {
            pid,
            process: process.to_string(),
            process_lc: process.to_lowercase(),
            power: format!("{power_num:.1}"),
            power_num,
        }
    }

    #[test]
    fn archive_bucket_metrics_returns_none_when_no_historical_samples_exist() {
        let samples = vec![None, None, None];
        assert_eq!(archive_bucket_metrics(&samples), None);
    }

    #[test]
    fn archive_bucket_metrics_summarizes_visible_bucket_values() {
        let samples = vec![None, Some(0.0), Some(2.0), Some(4.0), None];
        let metrics = archive_bucket_metrics(&samples).expect("metrics should be present");

        assert_eq!(metrics.bucket_count, 3);
        assert!((metrics.avg_bucket_power - 2.0).abs() < 1e-9);
        assert_eq!(metrics.max_bucket_power, 4.0);
    }

    #[test]
    fn record_history_trims_oversized_deque_back_to_limit() {
        let mut app = App::new();
        app.power_history = VecDeque::from(vec![1.0; HISTORY_LIMIT + 3]);
        app.snapshot = Snapshot {
            rows: Vec::new(),
            total_power: 9.0,
        };

        app.record_history();

        assert_eq!(app.power_history.len(), HISTORY_LIMIT);
        assert!(
            app.power_history
                .iter()
                .all(|value| *value == 1.0 || *value == 9.0)
        );
        assert_eq!(app.power_history.back().copied(), Some(9.0));
    }

    #[test]
    fn graph_heat_settings_validate_requires_ordered_thresholds() {
        let settings = GraphHeatSettings {
            yellow_start: 80.0,
            orange_start: 40.0,
            red_start: 120.0,
        };

        assert!(settings.validate().is_err());
    }

    #[test]
    fn spectrum_band_color_uses_absolute_breakpoints() {
        let settings = GraphHeatSettings {
            yellow_start: 20.0,
            orange_start: 40.0,
            red_start: 60.0,
        };

        assert_eq!(spectrum_band_color(10.0, &settings), COLOR_GREEN);
        assert_eq!(spectrum_band_color(25.0, &settings), COLOR_YELLOW);
        assert_eq!(spectrum_band_color(50.0, &settings), COLOR_ORANGE);
        assert_eq!(spectrum_band_color(70.0, &settings), COLOR_RED);
    }

    fn key_press(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), crossterm::event::KeyModifiers::NONE)
    }

    fn key_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
    }

    fn seed_process_sort_history(app: &mut App) {
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 60.0),
            row(20, "Mail", 20.0),
            row(30, "Slack", 5.0),
        ]));
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 1.0),
            row(20, "Mail", 20.0),
            row(30, "Slack", 6.0),
        ]));
    }

    fn process_identities_in_visible_order(app: &mut App) -> Vec<(i32, String)> {
        app.rebuild_process_rows_if_needed();
        app.process_rows
            .iter()
            .map(|row| (row.key.pid, row.key.process.clone()))
            .collect()
    }

    #[test]
    fn process_sort_cycles_with_s_key_and_wraps() {
        let mut app = App::new();

        assert_eq!(app.process_sort, app_state::ProcessSort::Current);

        app.handle_key(key_press('s'));
        assert_eq!(app.process_sort, app_state::ProcessSort::Avg2m);

        app.handle_key(key_press('s'));
        assert_eq!(app.process_sort, app_state::ProcessSort::Peak);

        app.handle_key(key_press('s'));
        assert_eq!(app.process_sort, app_state::ProcessSort::Current);
    }

    #[test]
    fn graph_range_cycles_with_r_key_and_wraps() {
        let mut app = App::new();

        assert_eq!(app.graph_range, GraphRange::Minutes8);

        app.handle_key(key_press('r'));
        assert_eq!(app.graph_range, GraphRange::Minutes30);

        app.handle_key(key_press('r'));
        assert_eq!(app.graph_range, GraphRange::Hours3);

        app.handle_key(key_press('r'));
        assert_eq!(app.graph_range, GraphRange::Hours12);

        app.handle_key(key_press('r'));
        assert_eq!(app.graph_range, GraphRange::Minutes8);
    }

    #[test]
    fn f_key_starts_filter_input_and_slash_does_not() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 4.0),
        ]));

        app.handle_key(key_press('f'));
        assert_eq!(app.process_filter_input.as_deref(), Some(""));

        app.handle_key(key_press('x'));
        assert_eq!(app.process_filter_input.as_deref(), Some("x"));

        app.handle_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.process_filter_input.is_none());

        app.handle_key(key_press('/'));
        assert!(app.process_filter_input.is_none());
    }

    #[test]
    fn main_graph_stays_live_only_even_when_range_changes() {
        let mut app = App::new();
        app.power_history = VecDeque::from(vec![1.0, 2.0, 3.0, 4.0, 5.0]);

        for range in [
            GraphRange::Minutes8,
            GraphRange::Minutes30,
            GraphRange::Hours3,
            GraphRange::Hours12,
        ] {
            app.graph_range = range;
            let samples = app.main_graph_live_samples_for_width(2);
            assert_eq!(
                samples,
                history_viewport_samples_deque(&app.power_history, 2)
            );
        }
    }

    #[test]
    fn pinned_process_history_uses_archive_pid_data_for_ranges_above_8m() {
        let mut app = App::new();
        let key = PidKey::new(10, "Safari");
        app.archive = persistence::ArchiveState {
            raw_2s: VecDeque::from(vec![persistence::TierSample {
                bucket_start_secs: 1_700,
                sample_count: 2,
                total_power_sum: 40.0,
                processes: vec![persistence::ArchivedProcessPower {
                    pid: 10,
                    process: "Safari".to_string(),
                    power_sum: 18.0,
                }],
                gap_before: false,
            }]),
            ..persistence::ArchiveState::default()
        };

        app.graph_range = GraphRange::Minutes8;
        assert!(app.pid_archive_samples_for_width(&key, 3).is_none());

        app.graph_range = GraphRange::Minutes30;
        let samples = app
            .pid_archive_samples_for_width(&key, 3)
            .expect("30m range should use exact-pid archive data");

        assert_eq!(samples.len(), 6);
        assert_eq!(samples[5], Some(9.0));
    }

    #[test]
    fn process_sort_changes_visible_ordering_by_metric() {
        let mut app = App::new();
        seed_process_sort_history(&mut app);

        assert_eq!(
            process_identities_in_visible_order(&mut app),
            vec![
                (20, "Mail".to_string()),
                (30, "Slack".to_string()),
                (10, "Safari".to_string()),
            ]
        );

        app.handle_key(key_press('s'));
        assert_eq!(app.process_sort, app_state::ProcessSort::Avg2m);
        assert_eq!(
            process_identities_in_visible_order(&mut app),
            vec![
                (10, "Safari".to_string()),
                (20, "Mail".to_string()),
                (30, "Slack".to_string()),
            ]
        );

        app.handle_key(key_press('s'));
        assert_eq!(app.process_sort, app_state::ProcessSort::Peak);
        assert_eq!(
            process_identities_in_visible_order(&mut app),
            vec![
                (10, "Safari".to_string()),
                (20, "Mail".to_string()),
                (30, "Slack".to_string()),
            ]
        );
    }

    #[test]
    fn enter_toggles_process_pin() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 4.0),
        ]));

        app.handle_key(key_enter());
        let pinned = app.pinned.as_ref().expect("process should be pinned");
        assert_eq!(pinned.pid, 10);
        assert_eq!(pinned.process, "Safari");

        app.handle_key(key_enter());
        assert!(app.pinned.is_none());
    }

    #[test]
    fn process_sort_cycle_preserves_selected_pid_when_possible() {
        let mut app = App::new();
        seed_process_sort_history(&mut app);

        app.process_selected = 1;
        let selected_before = app
            .selected_process_key()
            .expect("selected process should exist");
        assert_eq!(selected_before.pid, 30);

        app.handle_key(key_press('s'));

        let selected_after = app
            .selected_process_key()
            .expect("selected process should still exist");
        assert_eq!(selected_after.pid, 30);
        assert_eq!(app.process_selected, 2);
    }

    #[test]
    fn process_selection_survives_snapshot_reorder_by_metric() {
        let mut app = App::new();
        seed_process_sort_history(&mut app);

        app.process_selected = 1;
        assert_eq!(
            app.selected_process_key().as_ref().map(|key| key.pid),
            Some(30)
        );

        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 30.0),
            row(20, "Mail", 10.0),
            row(30, "Slack", 5.0),
        ]));

        assert_eq!(
            app.selected_process_key().as_ref().map(|key| key.pid),
            Some(30)
        );
        assert_eq!(app.process_selected, 2);
    }

    #[test]
    fn process_selection_tracks_exact_pid_even_with_same_name() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(11, "Safari", 6.0),
            row(20, "Mail", 4.0),
        ]));

        app.process_selected = 1;
        assert_eq!(
            app.selected_process_key().as_ref().map(|key| key.pid),
            Some(11)
        );

        app.apply_snapshot(snapshot_from_rows(vec![
            row(11, "Safari", 12.0),
            row(10, "Safari", 2.0),
            row(20, "Mail", 4.0),
        ]));

        assert_eq!(
            app.selected_process_key().as_ref().map(|key| key.pid),
            Some(11)
        );
    }

    #[test]
    fn process_filter_matches_name_or_pid() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(101, "Safari", 12.0),
            row(202, "Mail", 4.0),
            row(303, "Safari GPU", 3.0),
        ]));

        app.process_filter_query = "saf".to_string();
        app.mark_process_rows_dirty();
        let visible_len = app.process_visible_len();

        assert_eq!(visible_len, 2);
        assert_eq!(
            process_identities_in_visible_order(&mut app),
            vec![(101, "Safari".to_string()), (303, "Safari GPU".to_string())]
        );

        app.process_filter_query = "101".to_string();
        app.mark_process_rows_dirty();
        assert_eq!(
            process_identities_in_visible_order(&mut app),
            vec![(101, "Safari".to_string())]
        );
    }

    #[test]
    fn process_selection_normalizes_after_filter_changes_result_size() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 6.0),
            row(30, "Slack", 4.0),
        ]));

        assert_eq!(app.process_visible_len(), 3);

        app.process_selected = 2;
        app.process_scroll = 2;

        app.process_filter_query = "mail".to_string();
        app.mark_process_rows_dirty();
        let visible_len = app.process_visible_len();
        app.normalize_process_selection(visible_len);

        assert_eq!(visible_len, 1);
        assert_eq!(app.process_selected, 0);
        assert_eq!(app.process_scroll, 0);
    }

    #[test]
    fn app_graph_scale_recovers_after_spike_leaves_viewport() {
        let app = App::new();

        let width = 3;
        let spike_visible = history_viewport_samples(&[2.0, 180.0, 3.0, 4.0, 4.0, 4.0], width);
        let (_, spike_max) = app.graph_scale_bounds_for_viewport(&spike_visible);

        let spike_aged_out =
            history_viewport_samples(&[2.0, 180.0, 3.0, 4.0, 4.0, 4.0, 4.0, 4.0], width);
        let (_, recovered_max) = app.graph_scale_bounds_for_viewport(&spike_aged_out);
        let (_, expected_recovered_max) = graph_scale_bounds(&spike_aged_out);

        assert!(recovered_max < spike_max);
        assert_eq!(recovered_max, expected_recovered_max);
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

    #[test]
    fn loaded_session_cache_skips_live_hydration_when_gap_is_too_large() {
        let mut app = App::new();
        let mut archive = persistence::ArchiveState::default();
        archive.record_sample(
            100,
            persistence::continuity_threshold_secs(REFRESH_EVERY),
            12.0,
            [persistence::LiveProcessSample {
                pid: 10,
                process: "Safari".to_string(),
                power: 12.0,
            }],
        );

        app.apply_loaded_session_cache(persistence::LoadedSessionCache {
            cache: persistence::SessionCache {
                saved_at_unix_millis: 1,
                last_tick: 42,
                live_power_history: vec![7.0, 8.0],
                live_snapshots: vec![persistence::LiveSnapshot {
                    tick: 42,
                    samples: vec![persistence::LiveProcessSample {
                        pid: 10,
                        process: "Safari".to_string(),
                        power: 8.0,
                    }],
                }],
                archive: archive.clone(),
            },
            gap_millis: 99_999,
            hydrate_live: false,
        });

        assert!(app.power_history.is_empty());
        assert!(app.live_snapshot_history.is_empty());
        assert_eq!(app.tick, 0);
        assert_eq!(
            app.history_store.pid_current(&PidKey::new(10, "Safari")),
            0.0
        );
        assert_eq!(app.archive, archive);
        assert!(app.loading);
        assert!(app.snapshot.rows.is_empty());
    }
}
