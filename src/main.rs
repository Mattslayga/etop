use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    io::{self, BufRead, BufReader},
    process::{Child, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

mod history;
mod persistence;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use history::{HistoryStore, NameOffenderMetrics, PidKey, ProcessSample};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
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
const HISTORY_STALE_TICKS: u64 = HISTORY_LIMIT as u64;
const OFFENDER_AVG_WINDOW_TICKS: u64 = 60;
const OFFENDER_PEAK_WINDOW_TICKS: u64 = 60;
const PERSIST_FLUSH_EVERY_TICKS: u64 = 15;

const COLOR_FG: Color = Color::Rgb(0xab, 0xb2, 0xbf);
const COLOR_ACCENT: Color = Color::Rgb(0x61, 0xaf, 0xef);
const COLOR_MUTED: Color = Color::Rgb(0x5c, 0x63, 0x70);
const COLOR_SELECTED_BG: Color = Color::Rgb(0x2c, 0x31, 0x3c);
const COLOR_GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
const COLOR_YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const COLOR_ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
const COLOR_RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
const GRAPH_ACTIVITY_EPSILON: f64 = 1e-3;
const MAX_REASONABLE_POWER: f64 = 10_000.0;

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

#[derive(Debug, Clone)]
struct GraphHeatSettings {
    yellow_start: f64,
    orange_start: f64,
    red_start: f64,
}

impl Default for GraphHeatSettings {
    fn default() -> Self {
        Self {
            yellow_start: 40.0,
            orange_start: 85.0,
            red_start: 140.0,
        }
    }
}

impl GraphHeatSettings {
    fn validate(&self) -> Result<(), String> {
        let values = [self.yellow_start, self.orange_start, self.red_start];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err("thresholds must be finite numbers >= 0".to_string());
        }

        if !(self.yellow_start < self.orange_start && self.orange_start < self.red_start) {
            return Err("thresholds must satisfy yellow < orange < red".to_string());
        }

        Ok(())
    }

    fn color_for_power(&self, power: f64) -> Color {
        if power >= self.red_start {
            COLOR_RED
        } else if power >= self.orange_start {
            COLOR_ORANGE
        } else if power >= self.yellow_start {
            COLOR_YELLOW
        } else {
            COLOR_GREEN
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AppSettings {
    graph_heat: GraphHeatSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    YellowStart,
    OrangeStart,
    RedStart,
}

const SETTINGS_FIELDS: [SettingsField; 3] = [
    SettingsField::YellowStart,
    SettingsField::OrangeStart,
    SettingsField::RedStart,
];

impl SettingsField {
    fn label(self) -> &'static str {
        match self {
            SettingsField::YellowStart => "yellow start",
            SettingsField::OrangeStart => "orange start",
            SettingsField::RedStart => "red start",
        }
    }

    fn value(self, settings: &AppSettings) -> f64 {
        match self {
            SettingsField::YellowStart => settings.graph_heat.yellow_start,
            SettingsField::OrangeStart => settings.graph_heat.orange_start,
            SettingsField::RedStart => settings.graph_heat.red_start,
        }
    }

    fn set_value(self, settings: &mut AppSettings, value: f64) {
        match self {
            SettingsField::YellowStart => settings.graph_heat.yellow_start = value,
            SettingsField::OrangeStart => settings.graph_heat.orange_start = value,
            SettingsField::RedStart => settings.graph_heat.red_start = value,
        }
    }
}

#[derive(Debug, Clone)]
struct SettingsEditState {
    field: SettingsField,
    buffer: String,
}

#[derive(Debug, Clone)]
struct SettingsModalState {
    draft: AppSettings,
    selected: usize,
    editing: Option<SettingsEditState>,
    error: Option<String>,
}

impl SettingsModalState {
    fn new(current: &AppSettings) -> Self {
        Self {
            draft: current.clone(),
            selected: 0,
            editing: None,
            error: None,
        }
    }

    fn selected_field(&self) -> SettingsField {
        SETTINGS_FIELDS[self.selected.min(SETTINGS_FIELDS.len().saturating_sub(1))]
    }

    fn move_selection(&mut self, delta: isize) {
        if SETTINGS_FIELDS.is_empty() {
            self.selected = 0;
            return;
        }

        let len = SETTINGS_FIELDS.len() as isize;
        let current = self.selected as isize;
        self.selected = (current + delta).rem_euclid(len) as usize;
    }

    fn start_edit(&mut self) {
        let field = self.selected_field();
        let value = field.value(&self.draft);
        self.editing = Some(SettingsEditState {
            field,
            buffer: format_setting_value(value),
        });
        self.error = None;
    }

    fn display_value(&self, field: SettingsField) -> String {
        if let Some(edit) = self.editing.as_ref() {
            if edit.field == field {
                return edit.buffer.clone();
            }
        }

        format_setting_value(field.value(&self.draft))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableMode {
    Processes,
    Offenders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OffenderSort {
    Current,
    Avg2m,
    Peak,
    Share,
}

impl OffenderSort {
    fn next(self) -> Self {
        match self {
            Self::Current => Self::Avg2m,
            Self::Avg2m => Self::Peak,
            Self::Peak => Self::Share,
            Self::Share => Self::Current,
        }
    }

    fn title_label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Avg2m => "avg2m",
            Self::Peak => "peak",
            Self::Share => "share",
        }
    }

    fn sort_by_label(self) -> &'static str {
        match self {
            Self::Current => "NOW↓",
            Self::Avg2m => "AVG2M↓",
            Self::Peak => "PEAK↓",
            Self::Share => "SHARE↓",
        }
    }

    fn header_label(self, column: Self, default: &'static str) -> &'static str {
        if self == column {
            self.sort_by_label()
        } else {
            default
        }
    }

    fn compare(self, a: &NameOffenderMetrics, b: &NameOffenderMetrics) -> Ordering {
        let primary = match self {
            Self::Current => b.current.partial_cmp(&a.current),
            Self::Avg2m => b.avg.partial_cmp(&a.avg),
            Self::Peak => b.peak.partial_cmp(&a.peak),
            Self::Share => b.share.partial_cmp(&a.share),
        }
        .unwrap_or(Ordering::Equal);

        primary
            .then_with(|| b.current.partial_cmp(&a.current).unwrap_or(Ordering::Equal))
            .then_with(|| b.avg.partial_cmp(&a.avg).unwrap_or(Ordering::Equal))
            .then_with(|| b.peak.partial_cmp(&a.peak).unwrap_or(Ordering::Equal))
            .then_with(|| b.share.partial_cmp(&a.share).unwrap_or(Ordering::Equal))
            .then_with(|| a.name.cmp(&b.name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphRange {
    Minutes8,
    Minutes30,
    Hours3,
    Hours12,
}

impl GraphRange {
    fn next(self) -> Self {
        match self {
            Self::Minutes8 => Self::Minutes30,
            Self::Minutes30 => Self::Hours3,
            Self::Hours3 => Self::Hours12,
            Self::Hours12 => Self::Minutes8,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Minutes8 => "8m",
            Self::Minutes30 => "30m",
            Self::Hours3 => "3h",
            Self::Hours12 => "12h",
        }
    }

    fn archive_range(self) -> Option<persistence::ArchiveGraphRange> {
        match self {
            Self::Minutes8 => None,
            Self::Minutes30 => Some(persistence::ArchiveGraphRange::Minutes30),
            Self::Hours3 => Some(persistence::ArchiveGraphRange::Hours3),
            Self::Hours12 => Some(persistence::ArchiveGraphRange::Hours12),
        }
    }
}

enum MainGraphSamples {
    Live(Vec<f64>),
    Archive(Vec<Option<f64>>),
}

struct App {
    snapshot: Snapshot,
    last_error: Option<String>,
    loading: bool,
    paused: bool,
    settings: AppSettings,
    settings_modal: Option<SettingsModalState>,
    pinned: Option<PinnedProcess>,
    offender_pinned: Option<String>,
    process_selected: usize,
    process_scroll: usize,
    process_filter_query: String,
    process_filter_input: Option<String>,
    offender_selected: usize,
    offender_scroll: usize,
    offender_filter_query: String,
    offender_filter_input: Option<String>,
    power_history: VecDeque<f64>,
    live_snapshot_history: VecDeque<persistence::LiveSnapshot>,
    archive: persistence::ArchiveState,
    process_visible_indices: Vec<usize>,
    process_visible_dirty: bool,
    offender_rows: Vec<NameOffenderMetrics>,
    offender_visible_indices: Vec<usize>,
    offender_visible_dirty: bool,
    offender_sort: OffenderSort,
    graph_range: GraphRange,
    show_graph: bool,
    show_table: bool,
    table_mode: TableMode,
    tick: u64,
    history_store: HistoryStore,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            last_error: None,
            loading: true,
            paused: false,
            settings: AppSettings::default(),
            settings_modal: None,
            pinned: None,
            offender_pinned: None,
            process_selected: 0,
            process_scroll: 0,
            process_filter_query: String::new(),
            process_filter_input: None,
            offender_selected: 0,
            offender_scroll: 0,
            offender_filter_query: String::new(),
            offender_filter_input: None,
            power_history: VecDeque::with_capacity(HISTORY_LIMIT),
            live_snapshot_history: VecDeque::with_capacity(HISTORY_LIMIT),
            archive: persistence::ArchiveState::default(),
            process_visible_indices: Vec::new(),
            process_visible_dirty: true,
            offender_rows: Vec::new(),
            offender_visible_indices: Vec::new(),
            offender_visible_dirty: true,
            offender_sort: OffenderSort::Current,
            graph_range: GraphRange::Minutes8,
            show_graph: true,
            show_table: true,
            table_mode: TableMode::Processes,
            tick: 0,
            history_store: HistoryStore::new(HISTORY_LIMIT, HISTORY_STALE_TICKS),
        };

        let process_visible_len = app.process_visible_len();
        app.normalize_process_selection(process_visible_len);
        let offender_visible_len = app.offender_visible_len();
        app.normalize_offender_selection(offender_visible_len);
        app
    }

    fn process_active_filter(&self) -> &str {
        self.process_filter_input
            .as_deref()
            .unwrap_or(self.process_filter_query.as_str())
    }

    fn offender_active_filter(&self) -> &str {
        self.offender_filter_input
            .as_deref()
            .unwrap_or(self.offender_filter_query.as_str())
    }

    fn is_filter_input_active(&self) -> bool {
        match self.table_mode {
            TableMode::Processes => self.process_filter_input.is_some(),
            TableMode::Offenders => self.offender_filter_input.is_some(),
        }
    }

    fn mark_process_visible_dirty(&mut self) {
        self.process_visible_dirty = true;
    }

    fn mark_offender_visible_dirty(&mut self) {
        self.offender_visible_dirty = true;
    }

    fn rebuild_process_visible_if_needed(&mut self) {
        if !self.process_visible_dirty {
            return;
        }

        let filter_lc = self.process_active_filter().trim().to_lowercase();
        let has_filter = !filter_lc.is_empty();

        self.process_visible_indices.clear();
        self.process_visible_indices.reserve(
            self.snapshot
                .rows
                .len()
                .saturating_sub(self.process_visible_indices.len()),
        );

        for (idx, row) in self.snapshot.rows.iter().enumerate() {
            let matches = if !has_filter {
                true
            } else {
                row.process_lc.contains(&filter_lc) || row.pid.to_string().contains(&filter_lc)
            };

            if matches {
                self.process_visible_indices.push(idx);
            }
        }

        self.process_visible_indices.sort_by(|a, b| {
            let ra = &self.snapshot.rows[*a];
            let rb = &self.snapshot.rows[*b];

            rb.power_num
                .partial_cmp(&ra.power_num)
                .unwrap_or(Ordering::Equal)
                .then_with(|| ra.process.cmp(&rb.process))
        });

        self.process_visible_dirty = false;
    }

    fn rebuild_offender_visible_if_needed(&mut self) {
        if !self.offender_visible_dirty {
            return;
        }

        self.offender_rows = self
            .history_store
            .top_name_offenders(
                self.tick,
                OFFENDER_AVG_WINDOW_TICKS,
                OFFENDER_PEAK_WINDOW_TICKS,
                self.snapshot.total_power,
                usize::MAX,
            )
            .into_iter()
            .filter(|offender| {
                offender.current > GRAPH_ACTIVITY_EPSILON
                    || offender.avg > GRAPH_ACTIVITY_EPSILON
                    || offender.peak > GRAPH_ACTIVITY_EPSILON
            })
            .collect();

        self.offender_rows
            .sort_by(|a, b| self.offender_sort.compare(a, b));

        let filter_lc = self.offender_active_filter().trim().to_lowercase();
        let has_filter = !filter_lc.is_empty();

        self.offender_visible_indices.clear();
        self.offender_visible_indices.reserve(
            self.offender_rows
                .len()
                .saturating_sub(self.offender_visible_indices.len()),
        );

        for (idx, offender) in self.offender_rows.iter().enumerate() {
            let matches = if !has_filter {
                true
            } else {
                offender.name.to_lowercase().contains(&filter_lc)
            };

            if matches {
                self.offender_visible_indices.push(idx);
            }
        }

        self.offender_visible_dirty = false;
    }

    fn process_visible_len(&mut self) -> usize {
        self.rebuild_process_visible_if_needed();
        self.process_visible_indices.len()
    }

    fn offender_visible_len(&mut self) -> usize {
        self.rebuild_offender_visible_if_needed();
        self.offender_visible_indices.len()
    }

    fn visible_len(&mut self) -> usize {
        match self.table_mode {
            TableMode::Processes => self.process_visible_len(),
            TableMode::Offenders => self.offender_visible_len(),
        }
    }

    fn normalize_process_selection(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.process_selected = 0;
            self.process_scroll = 0;
            return;
        }

        if self.process_selected >= visible_len {
            self.process_selected = visible_len - 1;
        }

        if self.process_scroll > self.process_selected {
            self.process_scroll = self.process_selected;
        }
    }

    fn normalize_offender_selection(&mut self, visible_len: usize) {
        if visible_len == 0 {
            self.offender_selected = 0;
            self.offender_scroll = 0;
            return;
        }

        if self.offender_selected >= visible_len {
            self.offender_selected = visible_len - 1;
        }

        if self.offender_scroll > self.offender_selected {
            self.offender_scroll = self.offender_selected;
        }
    }

    fn normalize_selection(&mut self, visible_len: usize) {
        match self.table_mode {
            TableMode::Processes => self.normalize_process_selection(visible_len),
            TableMode::Offenders => self.normalize_offender_selection(visible_len),
        }
    }

    fn record_history(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.power_history.push_back(self.snapshot.total_power);

        while self.power_history.len() > HISTORY_LIMIT {
            let _ = self.power_history.pop_front();
        }

        let mut live_samples = Vec::with_capacity(self.snapshot.rows.len());
        let mut grouped_totals: HashMap<String, f64> = HashMap::new();

        for row in &self.snapshot.rows {
            live_samples.push(persistence::LiveProcessSample {
                pid: row.pid,
                process: row.process.clone(),
                power: row.power_num,
            });
            *grouped_totals.entry(row.process.clone()).or_insert(0.0) += row.power_num;
        }

        self.history_store.update(
            self.tick,
            live_samples.iter().map(|sample| ProcessSample {
                pid: sample.pid,
                process: sample.process.as_str(),
                power: sample.power,
            }),
        );

        self.live_snapshot_history
            .push_back(persistence::LiveSnapshot {
                tick: self.tick,
                samples: live_samples,
            });
        while self.live_snapshot_history.len() > HISTORY_LIMIT {
            let _ = self.live_snapshot_history.pop_front();
        }

        self.archive.record_sample(
            persistence::unix_time_secs_now(),
            persistence::continuity_threshold_secs(REFRESH_EVERY),
            self.snapshot.total_power,
            grouped_totals.into_iter(),
        );
    }

    fn rebuild_history_store_from_live_snapshots(&mut self) {
        let mut store = HistoryStore::new(HISTORY_LIMIT, HISTORY_STALE_TICKS);

        for snapshot in &self.live_snapshot_history {
            store.update(
                snapshot.tick,
                snapshot.samples.iter().map(|sample| ProcessSample {
                    pid: sample.pid,
                    process: sample.process.as_str(),
                    power: sample.power,
                }),
            );
        }

        self.history_store = store;
    }

    fn apply_loaded_session_cache(&mut self, loaded: persistence::LoadedSessionCache) {
        let persistence::LoadedSessionCache {
            mut cache,
            gap_millis: _gap_millis,
            hydrate_live,
        } = loaded;

        self.archive = cache.archive;

        if !hydrate_live {
            // Restore the archive foundation, but leave live buffers empty so the UI does
            // not imply sampling continued across a real downtime gap.
            return;
        }

        if cache.live_power_history.len() > HISTORY_LIMIT {
            let drop = cache.live_power_history.len() - HISTORY_LIMIT;
            cache.live_power_history.drain(0..drop);
        }

        cache.live_snapshots.sort_by_key(|snapshot| snapshot.tick);
        if cache.live_snapshots.len() > HISTORY_LIMIT {
            let drop = cache.live_snapshots.len() - HISTORY_LIMIT;
            cache.live_snapshots.drain(0..drop);
        }

        self.power_history = VecDeque::from(cache.live_power_history);
        self.live_snapshot_history = VecDeque::from(cache.live_snapshots);
        self.tick = cache.last_tick;
        if let Some(last_tick) = self
            .live_snapshot_history
            .back()
            .map(|snapshot| snapshot.tick)
        {
            self.tick = self.tick.max(last_tick);
        }

        self.rebuild_history_store_from_live_snapshots();

        if let Some(last_snapshot) = self.live_snapshot_history.back() {
            self.snapshot = snapshot_from_live_snapshot(last_snapshot);
            self.loading = false;
            self.last_error = None;
            self.mark_process_visible_dirty();
            self.mark_offender_visible_dirty();
            let process_visible_len = self.process_visible_len();
            self.normalize_process_selection(process_visible_len);
            let offender_visible_len = self.offender_visible_len();
            self.normalize_offender_selection(offender_visible_len);
        }
    }

    fn to_session_cache(&self) -> persistence::SessionCache {
        persistence::SessionCache {
            saved_at_unix_millis: persistence::unix_time_millis_now(),
            last_tick: self.tick,
            live_power_history: self.power_history.iter().copied().collect(),
            live_snapshots: self.live_snapshot_history.iter().cloned().collect(),
            archive: self.archive.clone(),
        }
    }

    fn cycle_graph_range(&mut self) {
        self.graph_range = self.graph_range.next();
    }

    fn main_graph_samples_for_width(&self, graph_width: usize, now_secs: u64) -> MainGraphSamples {
        match self.graph_range.archive_range() {
            None => MainGraphSamples::Live(history_viewport_samples_deque(
                &self.power_history,
                graph_width,
            )),
            Some(archive_range) => MainGraphSamples::Archive(self.archive.graph_samples_for_range(
                archive_range,
                graph_width,
                now_secs,
            )),
        }
    }

    fn graph_scale_bounds_for_viewport(&self, samples: &[f64]) -> (f64, f64) {
        graph_scale_bounds(samples)
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible_len();
        if len == 0 {
            match self.table_mode {
                TableMode::Processes => {
                    self.process_selected = 0;
                    self.process_scroll = 0;
                }
                TableMode::Offenders => {
                    self.offender_selected = 0;
                    self.offender_scroll = 0;
                }
            }
            return;
        }

        let current = match self.table_mode {
            TableMode::Processes => self.process_selected,
            TableMode::Offenders => self.offender_selected,
        };
        let max = (len - 1) as isize;
        let next = (current as isize + delta).clamp(0, max) as usize;

        match self.table_mode {
            TableMode::Processes => self.process_selected = next,
            TableMode::Offenders => self.offender_selected = next,
        }
    }

    fn select_top(&mut self) {
        match self.table_mode {
            TableMode::Processes => {
                self.process_selected = 0;
                self.process_scroll = 0;
            }
            TableMode::Offenders => {
                self.offender_selected = 0;
                self.offender_scroll = 0;
            }
        }
    }

    fn select_bottom(&mut self) {
        let len = self.visible_len();
        match self.table_mode {
            TableMode::Processes => {
                if len == 0 {
                    self.process_selected = 0;
                    self.process_scroll = 0;
                } else {
                    self.process_selected = len - 1;
                }
            }
            TableMode::Offenders => {
                if len == 0 {
                    self.offender_selected = 0;
                    self.offender_scroll = 0;
                } else {
                    self.offender_selected = len - 1;
                }
            }
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    fn is_process_table_mode(&self) -> bool {
        self.table_mode == TableMode::Processes
    }

    fn toggle_table_mode(&mut self) {
        self.table_mode = match self.table_mode {
            TableMode::Processes => TableMode::Offenders,
            TableMode::Offenders => TableMode::Processes,
        };
    }

    fn selected_offender_name(&mut self) -> Option<String> {
        self.rebuild_offender_visible_if_needed();
        let idx = self
            .offender_visible_indices
            .get(self.offender_selected)
            .copied()?;
        self.offender_rows
            .get(idx)
            .map(|offender| offender.name.clone())
    }

    fn restore_offender_selection_by_name(&mut self, selected_name: Option<String>) {
        self.rebuild_offender_visible_if_needed();

        if let Some(name) = selected_name {
            if let Some(next_selected) = self
                .offender_visible_indices
                .iter()
                .position(|idx| self.offender_rows[*idx].name == name)
            {
                self.offender_selected = next_selected;
            }
        }

        self.normalize_offender_selection(self.offender_visible_indices.len());
    }

    fn cycle_offender_sort(&mut self) {
        let selected_name = self.selected_offender_name();
        self.offender_sort = self.offender_sort.next();
        self.mark_offender_visible_dirty();
        self.restore_offender_selection_by_name(selected_name);
    }

    fn is_pinned(&self) -> bool {
        self.is_process_table_mode() && self.pinned.is_some()
    }

    fn is_offender_pinned(&self) -> bool {
        self.table_mode == TableMode::Offenders && self.offender_pinned.is_some()
    }

    fn toggle_pin(&mut self) {
        if !self.is_process_table_mode() {
            return;
        }

        if self.pinned.is_some() {
            self.pinned = None;
            return;
        }

        self.rebuild_process_visible_if_needed();
        let Some(snapshot_idx) = self
            .process_visible_indices
            .get(self.process_selected)
            .copied()
        else {
            return;
        };

        if let Some(row) = self.snapshot.rows.get(snapshot_idx) {
            self.pinned = Some(PinnedProcess {
                pid: row.pid,
                process: row.process.clone(),
            });
        }
    }

    fn toggle_offender_pin(&mut self) {
        if self.table_mode != TableMode::Offenders {
            return;
        }

        if self.offender_pinned.is_some() {
            self.offender_pinned = None;
            return;
        }

        self.offender_pinned = self.selected_offender_name();
    }

    fn open_settings_modal(&mut self) {
        self.settings_modal = Some(SettingsModalState::new(&self.settings));
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        let mut close_modal = false;
        let mut apply_settings: Option<AppSettings> = None;

        let Some(modal) = self.settings_modal.as_mut() else {
            return;
        };

        if let Some(edit) = modal.editing.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    modal.editing = None;
                    modal.error = None;
                }
                KeyCode::Enter => {
                    let parsed = edit.buffer.trim().parse::<f64>();
                    match parsed {
                        Ok(value) if value.is_finite() && value >= 0.0 => {
                            edit.field.set_value(&mut modal.draft, value);
                            modal.editing = None;
                            modal.error = None;
                        }
                        Ok(_) => {
                            modal.error = Some("value must be a finite number >= 0".to_string());
                        }
                        Err(_) => {
                            modal.error = Some("invalid number".to_string());
                        }
                    }
                }
                KeyCode::Backspace => {
                    edit.buffer.pop();
                    modal.error = None;
                }
                KeyCode::Char(ch)
                    if ch.is_ascii_digit()
                        || ch == '.'
                        || (ch == '-' && edit.buffer.is_empty()) =>
                {
                    edit.buffer.push(ch);
                    modal.error = None;
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    close_modal = true;
                }
                KeyCode::Char('m') | KeyCode::Char('M') => {
                    match modal.draft.graph_heat.validate() {
                        Ok(()) => {
                            apply_settings = Some(modal.draft.clone());
                            close_modal = true;
                        }
                        Err(err) => {
                            modal.error = Some(err);
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    modal.move_selection(1);
                    modal.error = None;
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    modal.move_selection(-1);
                    modal.error = None;
                }
                KeyCode::Enter => {
                    modal.start_edit();
                }
                _ => {}
            }
        }

        if let Some(next_settings) = apply_settings {
            self.settings = next_settings;
        }

        if close_modal {
            self.settings_modal = None;
        }
    }

    fn start_filter_input(&mut self) {
        match self.table_mode {
            TableMode::Processes => {
                self.process_filter_input = Some(self.process_filter_query.clone());
                self.mark_process_visible_dirty();
            }
            TableMode::Offenders => {
                self.offender_filter_input = Some(self.offender_filter_query.clone());
                self.mark_offender_visible_dirty();
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match self.table_mode {
            TableMode::Processes => {
                let Some(buf) = self.process_filter_input.as_mut() else {
                    return;
                };

                let mut touched_filter = false;

                match key.code {
                    KeyCode::Esc => {
                        self.process_filter_input = None;
                        self.process_selected = 0;
                        self.process_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Enter => {
                        self.process_filter_query = buf.clone();
                        self.process_filter_input = None;
                        self.process_selected = 0;
                        self.process_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        self.process_selected = 0;
                        self.process_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Char(ch) => {
                        buf.push(ch);
                        self.process_selected = 0;
                        self.process_scroll = 0;
                        touched_filter = true;
                    }
                    _ => {}
                }

                if touched_filter {
                    self.mark_process_visible_dirty();
                }

                let visible_len = self.process_visible_len();
                self.normalize_process_selection(visible_len);
            }
            TableMode::Offenders => {
                let Some(buf) = self.offender_filter_input.as_mut() else {
                    return;
                };

                let mut touched_filter = false;

                match key.code {
                    KeyCode::Esc => {
                        self.offender_filter_input = None;
                        self.offender_selected = 0;
                        self.offender_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Enter => {
                        self.offender_filter_query = buf.clone();
                        self.offender_filter_input = None;
                        self.offender_selected = 0;
                        self.offender_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        self.offender_selected = 0;
                        self.offender_scroll = 0;
                        touched_filter = true;
                    }
                    KeyCode::Char(ch) => {
                        buf.push(ch);
                        self.offender_selected = 0;
                        self.offender_scroll = 0;
                        touched_filter = true;
                    }
                    _ => {}
                }

                if touched_filter {
                    self.mark_offender_visible_dirty();
                }

                let visible_len = self.offender_visible_len();
                self.normalize_offender_selection(visible_len);
            }
        }
    }

    fn apply_snapshot(&mut self, next: Snapshot) {
        let selected_offender_name = self.selected_offender_name();

        self.snapshot = next;
        self.last_error = None;
        self.loading = false;
        self.mark_process_visible_dirty();
        self.mark_offender_visible_dirty();
        self.record_history();

        let process_visible_len = self.process_visible_len();
        self.normalize_process_selection(process_visible_len);
        self.restore_offender_selection_by_name(selected_offender_name);
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

    fn status_hint_text(&self) -> &'static str {
        if self.settings_modal.is_some() {
            "Enter edit • Esc cancel"
        } else if self.is_filter_input_active() {
            "Enter apply • Esc cancel"
        } else if self.table_mode == TableMode::Offenders {
            if self.offender_pinned.is_some() {
                "Enter unpin • / filter • s sort • 3 processes • 4 range • space pause"
            } else {
                "j/k move • g/G jump • / filter • Enter pin • s sort • 3 processes • 4 range • space pause"
            }
        } else if self.pinned.is_some() {
            "Enter unpin • / filter • 3 offenders • 4 range • space pause"
        } else {
            "j/k move • g/G jump • / filter • Enter pin • 3 offenders • 4 range • space pause"
        }
    }

    fn status_hint_chips(&self) -> Vec<(&'static str, char)> {
        if self.settings_modal.is_some() {
            vec![("menu", 'm')]
        } else if self.is_filter_input_active() {
            vec![]
        } else {
            vec![("menu", 'm'), ("filter /", '/'), ("quit", 'q')]
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }

        if self.settings_modal.is_some() {
            self.handle_settings_key(key);
            return false;
        }

        if self.is_filter_input_active() {
            self.handle_filter_key(key);
            return false;
        }

        let can_navigate = match self.table_mode {
            TableMode::Processes => !self.is_pinned(),
            TableMode::Offenders => !self.is_offender_pinned(),
        };

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return true,
            KeyCode::Char('j') | KeyCode::Down if can_navigate => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up if can_navigate => self.move_selection(-1),
            KeyCode::Char('g') if can_navigate => self.select_top(),
            KeyCode::Char('G') if can_navigate => self.select_bottom(),
            KeyCode::Char('/') => self.start_filter_input(),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') | KeyCode::Char('M') => self.open_settings_modal(),
            KeyCode::Char('1') => self.show_graph = !self.show_graph,
            KeyCode::Char('2') => self.show_table = !self.show_table,
            KeyCode::Char('3') => self.toggle_table_mode(),
            KeyCode::Char('4') => self.cycle_graph_range(),
            KeyCode::Char('s') | KeyCode::Char('S') if self.table_mode == TableMode::Offenders => {
                self.cycle_offender_sort();
            }
            KeyCode::Enter => match self.table_mode {
                TableMode::Processes => self.toggle_pin(),
                TableMode::Offenders => self.toggle_offender_pin(),
            },
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

fn format_setting_value(value: f64) -> String {
    let mut s = format!("{value:.1}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }

    if s.is_empty() { "0".to_string() } else { s }
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
    let points = width.saturating_mul(2);
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

fn history_viewport_samples_deque(values: &VecDeque<f64>, width: usize) -> Vec<f64> {
    let points = width.saturating_mul(2);
    if points == 0 {
        return Vec::new();
    }

    if values.is_empty() {
        return vec![0.0; points];
    }

    let tail_len = values.len().min(points);
    let mut samples = vec![0.0; points];
    let start = points - tail_len;

    for (dst, value) in samples[start..]
        .iter_mut()
        .zip(values.iter().skip(values.len() - tail_len))
    {
        *dst = *value;
    }

    samples
}

fn history_viewport_samples_optional(values: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    let points = width.saturating_mul(2);
    if points == 0 {
        return Vec::new();
    }

    if values.is_empty() {
        return vec![None; points];
    }

    if values.len() >= points {
        return values[values.len() - points..].to_vec();
    }

    let mut samples = vec![None; points];
    let start = points - values.len();
    samples[start..].clone_from_slice(values);
    samples
}

fn history_range_optional(values: &[Option<f64>]) -> Option<(f64, f64)> {
    let mut iter = values.iter().flatten().copied();
    let first = iter.next()?;

    let mut min = first;
    let mut max = first;

    for value in iter {
        min = min.min(value);
        max = max.max(value);
    }

    Some((min, max))
}

fn graph_scale_bounds_optional(values: &[Option<f64>]) -> (f64, f64) {
    let (raw_min, raw_max) = history_range_optional(values).unwrap_or((0.0, 0.0));

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
    let activity = (value - min).max(0.0);

    if activity <= GRAPH_ACTIVITY_EPSILON {
        return 0;
    }

    let normalized = (activity / (max - min)).clamp(0.0, 1.0);
    ((normalized * max_steps as f64).round() as i32).max(1)
}

fn spectrum_band_color(power: f64, thresholds: &GraphHeatSettings) -> Color {
    thresholds.color_for_power(power)
}

fn graph_span_style(color: Option<Color>) -> Style {
    match color {
        Some(color) => Style::default().fg(color),
        None => Style::default(),
    }
}

fn row_position_color(row_from_top: usize, height: usize) -> Color {
    if height <= 1 {
        return COLOR_GREEN;
    }

    let row_from_bottom = height - 1 - row_from_top;
    let fraction = row_from_bottom as f64 / (height - 1) as f64;

    if fraction >= 0.85 {
        COLOR_RED
    } else if fraction >= 0.65 {
        COLOR_ORANGE
    } else if fraction >= 0.40 {
        COLOR_YELLOW
    } else {
        COLOR_GREEN
    }
}

fn braille_history_cells_with_scale(
    values: &[f64],
    width: usize,
    height: usize,
    scale_min: f64,
    scale_max: f64,
) -> Vec<Vec<(char, Color)>> {
    if height == 0 {
        return Vec::new();
    }

    if width == 0 {
        return vec![Vec::new(); height];
    }

    let samples = history_viewport_samples(values, width);

    let steps: Vec<i32> = samples
        .iter()
        .map(|value| value_to_vertical_steps(*value, scale_min, scale_max, height))
        .collect();

    let mut rows = Vec::with_capacity(height);

    for row_from_top in 0..height {
        let row_from_bottom = height - 1 - row_from_top;
        let row_base = (row_from_bottom * 4) as i32;
        let row_color = row_position_color(row_from_top, height);

        let mut line = Vec::with_capacity(width);
        for col in 0..width {
            let left_level = (steps[col * 2] - row_base).clamp(0, 4) as usize;
            let right_level = (steps[col * 2 + 1] - row_base).clamp(0, 4) as usize;
            line.push((BRAILLE_5X5[left_level * 5 + right_level], row_color));
        }

        rows.push(line);
    }

    let bottom_row_color = row_position_color(height - 1, height);
    for col in 0..width {
        let has_visible_segment = rows.iter().any(|row| row[col].0 != ' ');
        if has_visible_segment {
            continue;
        }

        let left_active = (samples[col * 2] - scale_min).max(0.0) > GRAPH_ACTIVITY_EPSILON;
        let right_active = (samples[col * 2 + 1] - scale_min).max(0.0) > GRAPH_ACTIVITY_EPSILON;
        if !left_active && !right_active {
            continue;
        }

        rows[height - 1][col] = (
            BRAILLE_5X5[usize::from(left_active) * 5 + usize::from(right_active)],
            bottom_row_color,
        );
    }

    rows
}

#[cfg(test)]
fn braille_history_cells(values: &[f64], width: usize, height: usize) -> Vec<Vec<(char, Color)>> {
    let samples = history_viewport_samples(values, width);
    let (scale_min, scale_max) = graph_scale_bounds(&samples);
    braille_history_cells_with_scale(values, width, height, scale_min, scale_max)
}

fn braille_history_lines_with_scale(
    values: &[f64],
    width: usize,
    height: usize,
    scale_min: f64,
    scale_max: f64,
) -> Vec<Line<'static>> {
    braille_history_cells_with_scale(values, width, height, scale_min, scale_max)
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;

            for (ch, cell_color) in row {
                let color = if ch == ' ' { None } else { Some(cell_color) };

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

fn braille_history_cells_optional_with_scale(
    values: &[Option<f64>],
    width: usize,
    height: usize,
    scale_min: f64,
    scale_max: f64,
) -> Vec<Vec<(char, Color)>> {
    if height == 0 {
        return Vec::new();
    }

    if width == 0 {
        return vec![Vec::new(); height];
    }

    let samples = history_viewport_samples_optional(values, width);

    let steps: Vec<Option<i32>> = samples
        .iter()
        .map(|value| {
            value.map(|value| value_to_vertical_steps(value, scale_min, scale_max, height))
        })
        .collect();

    let mut rows = Vec::with_capacity(height);

    for row_from_top in 0..height {
        let row_from_bottom = height - 1 - row_from_top;
        let row_base = (row_from_bottom * 4) as i32;
        let row_color = row_position_color(row_from_top, height);

        let mut line = Vec::with_capacity(width);
        for col in 0..width {
            let left_step = steps[col * 2];
            let right_step = steps[col * 2 + 1];

            if left_step.is_none() && right_step.is_none() {
                line.push((' ', row_color));
                continue;
            }

            let left_level = left_step.map(|step| (step - row_base).clamp(0, 4) as usize);
            let right_level = right_step.map(|step| (step - row_base).clamp(0, 4) as usize);

            line.push((
                BRAILLE_5X5[left_level.unwrap_or(0) * 5 + right_level.unwrap_or(0)],
                row_color,
            ));
        }

        rows.push(line);
    }

    let bottom_row_color = row_position_color(height - 1, height);
    for col in 0..width {
        let has_visible_segment = rows.iter().any(|row| row[col].0 != ' ');
        if has_visible_segment {
            continue;
        }

        let left_active = samples[col * 2]
            .map(|value| (value - scale_min).max(0.0) > GRAPH_ACTIVITY_EPSILON)
            .unwrap_or(false);
        let right_active = samples[col * 2 + 1]
            .map(|value| (value - scale_min).max(0.0) > GRAPH_ACTIVITY_EPSILON)
            .unwrap_or(false);
        if !left_active && !right_active {
            continue;
        }

        rows[height - 1][col] = (
            BRAILLE_5X5[usize::from(left_active) * 5 + usize::from(right_active)],
            bottom_row_color,
        );
    }

    rows
}

fn braille_history_lines_optional_with_scale(
    values: &[Option<f64>],
    width: usize,
    height: usize,
    scale_min: f64,
    scale_max: f64,
) -> Vec<Line<'static>> {
    braille_history_cells_optional_with_scale(values, width, height, scale_min, scale_max)
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;

            for (ch, cell_color) in row {
                let color = if ch == ' ' { None } else { Some(cell_color) };

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
fn braille_history_lines(values: &[f64], width: usize, height: usize) -> Vec<Line<'static>> {
    let samples = history_viewport_samples(values, width);
    let (scale_min, scale_max) = graph_scale_bounds(&samples);
    braille_history_lines_with_scale(values, width, height, scale_min, scale_max)
}

#[cfg(test)]
fn braille_history_rows(values: &[f64], width: usize, height: usize) -> Vec<String> {
    braille_history_cells(values, width, height)
        .into_iter()
        .map(|row| row.into_iter().map(|(ch, _)| ch).collect())
        .collect()
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    app.rebuild_process_visible_if_needed();
    app.rebuild_offender_visible_if_needed();

    let process_visible_len = app.process_visible_indices.len();
    let offender_visible_len = app.offender_visible_indices.len();
    app.normalize_process_selection(process_visible_len);
    app.normalize_offender_selection(offender_visible_len);

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
        app.process_visible_indices.iter().position(|idx| {
            app.snapshot.rows[*idx].pid == pin.pid && app.snapshot.rows[*idx].process == pin.process
        })
    });

    let offender_pinned = app.offender_pinned.clone();
    let offender_pinned_visible_rank = offender_pinned.as_ref().and_then(|name| {
        app.offender_visible_indices
            .iter()
            .position(|idx| app.offender_rows[*idx].name == *name)
    });

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

    if let Some(graph_area) = graph_slot {
        let graph_inner = panel_block().inner(graph_area);
        let graph_width = graph_inner.width as usize;
        let graph_height = graph_inner.height as usize;

        let (graph_lines, graph_point_count, scale_min, scale_max) = match app
            .main_graph_samples_for_width(graph_width, persistence::unix_time_secs_now())
        {
            MainGraphSamples::Live(graph_samples) => {
                let (scale_min, scale_max) = app.graph_scale_bounds_for_viewport(&graph_samples);
                let graph_lines = braille_history_lines_with_scale(
                    &graph_samples,
                    graph_width,
                    graph_height,
                    scale_min,
                    scale_max,
                );

                (graph_lines, app.power_history.len(), scale_min, scale_max)
            }
            MainGraphSamples::Archive(graph_samples) => {
                let (scale_min, scale_max) = graph_scale_bounds_optional(&graph_samples);
                let graph_lines = braille_history_lines_optional_with_scale(
                    &graph_samples,
                    graph_width,
                    graph_height,
                    scale_min,
                    scale_max,
                );
                let graph_point_count =
                    graph_samples.iter().filter(|value| value.is_some()).count();

                (graph_lines, graph_point_count, scale_min, scale_max)
            }
        };

        let graph_title_right = Line::from(vec![
            Span::styled("power ", Style::default().fg(COLOR_MUTED)),
            Span::styled(
                format!("{:.1}", app.snapshot.total_power),
                Style::default().fg(COLOR_RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " • {} • {} pts • {:.1}–{:.1}",
                    app.graph_range.label(),
                    graph_point_count,
                    scale_min,
                    scale_max
                ),
                Style::default().fg(COLOR_MUTED),
            ),
        ])
        .right_aligned();

        let mut graph_bottom_spans = hotkey_hint_line("4 range • space pause").spans;
        if let Some(error) = app.last_error.as_deref() {
            graph_bottom_spans.push(Span::styled(" • ", Style::default().fg(COLOR_MUTED)));
            graph_bottom_spans.push(Span::styled(
                format!("error: {error}"),
                Style::default().fg(COLOR_RED),
            ));
        }
        let graph_block = panel_block()
            .title_top(graph_title_right)
            .title_bottom(Line::from(graph_bottom_spans).right_aligned());
        frame.render_widget(graph_block, graph_area);

        let graph_chips = vec![
            chip_line("¹etop", Some('¹')),
            chip_line(&format!("4{}", app.graph_range.label()), Some('4')),
            {
                let mut spans = chip_line(mode, None);
                for span in &mut spans {
                    span.style = span.style.patch(mode_style);
                }
                spans
            },
            {
                let mut spans = chip_line(load_state, None);
                for span in &mut spans {
                    span.style = span.style.patch(load_style);
                }
                spans
            },
        ];
        let border_y = graph_area.y;
        draw_chips_on_border(
            frame.buffer_mut(),
            graph_area,
            border_y,
            graph_area.x + 1,
            &graph_chips,
        );

        let graph_bottom_chips = vec![chip_line("menu", Some('m'))];
        let bottom_y = graph_area.y + graph_area.height.saturating_sub(1);
        draw_chips_on_border(
            frame.buffer_mut(),
            graph_area,
            bottom_y,
            graph_area.x + 1,
            &graph_bottom_chips,
        );

        debug_assert!(graph_height > 0 || graph_width == 0);
        let graph = Paragraph::new(graph_lines);
        frame.render_widget(graph, graph_inner);
    }

    let Some(table_region) = table_slot else {
        if let Some(modal) = app.settings_modal.as_ref() {
            draw_settings_modal(frame, modal);
        }
        return;
    };
    let show_process_table = app.table_mode == TableMode::Processes;

    let show_detail = if show_process_table {
        pinned.is_some()
    } else {
        offender_pinned.is_some()
    };

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

    if show_process_table {
        if let Some(detail_rect) = detail_area {
            let detail_title = if let Some(pin) = pinned.as_ref() {
                format!("pinned {}", pin.pid)
            } else {
                "pinned process".to_string()
            };

            let detail_block = panel_block()
                .title_top(detail_title)
                .title_bottom(hotkey_hint_line("Enter unpin").right_aligned());
            let detail_inner = detail_block.inner(detail_rect);
            frame.render_widget(detail_block, detail_rect);

            let text_lines: Vec<Line> = match (pinned.as_ref(), pinned_row.as_ref()) {
                (Some(_), Some(row)) => {
                    let power_share = if app.snapshot.total_power > 0.0 {
                        (row.power_num / app.snapshot.total_power) * 100.0
                    } else {
                        0.0
                    };

                    let rank_text = pinned_rank
                        .map(|rank| format!("#{} / {}", rank + 1, process_visible_len))
                        .unwrap_or_else(|| "not in current filtered list".to_string());

                    vec![
                        Line::from(Span::styled(
                            row.process.clone(),
                            Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(vec![
                            Span::styled("pid ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(row.pid.to_string(), Style::default().fg(COLOR_FG)),
                            Span::styled("  pwr ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                row.power.clone(),
                                Style::default()
                                    .fg(spectrum_band_color(
                                        row.power_num,
                                        &app.settings.graph_heat,
                                    ))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled("  share ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                format!("{power_share:.1}%"),
                                Style::default().fg(COLOR_FG),
                            ),
                            Span::styled("  rank ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(rank_text, Style::default().fg(COLOR_FG)),
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
                ],
                _ => vec![],
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

        let graph_heat = app.settings.graph_heat.clone();
        let rows = app.process_visible_indices[start..end].iter().map(|idx| {
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
                Cell::from(r.power.clone())
                    .style(Style::default().fg(spectrum_band_color(r.power_num, &graph_heat))),
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

        let table_title_right = if app.process_filter_input.is_some() {
            format!(
                "{process_visible_len}/{} • filter edit: {}{}",
                app.snapshot.rows.len(),
                app.process_active_filter(),
                pin_suffix,
            )
        } else if app.process_filter_query.is_empty() {
            format!(
                "{process_visible_len}/{} • power ↓{}",
                app.snapshot.rows.len(),
                pin_suffix,
            )
        } else {
            format!(
                "{process_visible_len}/{} • power ↓ • filter: {}{}",
                app.snapshot.rows.len(),
                app.process_filter_query,
                pin_suffix,
            )
        };

        let highlight_style = if app.is_pinned() {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
                .bg(COLOR_SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        };

        let table_block = panel_block()
            .title_top(
                Line::from(Span::styled(
                    table_title_right,
                    Style::default().fg(COLOR_MUTED),
                ))
                .right_aligned(),
            )
            .title_bottom(hotkey_hint_line(app.status_hint_text()).right_aligned());

        let table = Table::new(
            rows,
            [
                Constraint::Length(7),
                Constraint::Percentage(70),
                Constraint::Length(10),
            ],
        )
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
    } else {
        if let Some(detail_rect) = detail_area {
            let detail_title = if let Some(name) = offender_pinned.as_ref() {
                format!("pinned {name}")
            } else {
                "pinned offender".to_string()
            };

            let detail_block = panel_block()
                .title_top(detail_title)
                .title_bottom(hotkey_hint_line("Enter unpin").right_aligned());
            let detail_inner = detail_block.inner(detail_rect);
            frame.render_widget(detail_block, detail_rect);

            let text_lines: Vec<Line> = match offender_pinned.as_ref() {
                Some(name) => {
                    let current = app.history_store.name_current(name);
                    let avg = app
                        .history_store
                        .name_avg(name, OFFENDER_AVG_WINDOW_TICKS, app.tick);
                    let peak =
                        app.history_store
                            .name_peak(name, OFFENDER_PEAK_WINDOW_TICKS, app.tick);
                    let share =
                        app.history_store.name_share(name, app.snapshot.total_power) * 100.0;

                    let rank_text = offender_pinned_visible_rank
                        .map(|rank| format!("#{} / {}", rank + 1, offender_visible_len))
                        .unwrap_or_else(|| "not in current filtered list".to_string());

                    let present_in_current = current > GRAPH_ACTIVITY_EPSILON;

                    let mut lines = vec![
                        Line::from(Span::styled(
                            name.clone(),
                            Style::default().fg(COLOR_FG).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(vec![
                            Span::styled("now ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(
                                format!("{current:.1}"),
                                Style::default()
                                    .fg(spectrum_band_color(current, &app.settings.graph_heat))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled("  avg2m ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(format!("{avg:.1}"), Style::default().fg(COLOR_FG)),
                            Span::styled("  peak ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(format!("{peak:.1}"), Style::default().fg(COLOR_FG)),
                            Span::styled("  share ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(format!("{share:.1}%"), Style::default().fg(COLOR_FG)),
                            Span::styled("  rank ", Style::default().fg(COLOR_MUTED)),
                            Span::styled(rank_text, Style::default().fg(COLOR_FG)),
                        ]),
                    ];

                    if !present_in_current {
                        lines.push(Line::from(Span::styled(
                            "Not present in the latest grouped sample.",
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

            if let (Some(graph_rect), Some(name)) = (mini_graph_rect, offender_pinned.as_ref()) {
                let graph_w = graph_rect.width as usize;
                let graph_h = graph_rect.height as usize;
                if graph_w > 0 && graph_h > 0 {
                    let history_samples =
                        app.history_store
                            .name_recent_values(name, HISTORY_LIMIT as u64, app.tick);
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

        let rows_visible = rows_area.height.saturating_sub(3) as usize;
        let offender_total = app.offender_rows.len();

        if rows_visible > 0 && offender_visible_len > 0 {
            if app.offender_selected < app.offender_scroll {
                app.offender_scroll = app.offender_selected;
            }
            if app.offender_selected >= app.offender_scroll + rows_visible {
                app.offender_scroll = app.offender_selected + 1 - rows_visible;
            }
        } else {
            app.offender_scroll = 0;
        }

        let start = app.offender_scroll.min(offender_visible_len);
        let end = if rows_visible == 0 {
            start
        } else {
            (start + rows_visible).min(offender_visible_len)
        };

        let rows = app.offender_visible_indices[start..end].iter().map(|idx| {
            let offender = &app.offender_rows[*idx];
            let is_pinned_row = offender_pinned
                .as_ref()
                .map(|name| name == &offender.name)
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
                Cell::from(offender.name.clone()),
                Cell::from(format!("{:.1}", offender.current)).style(Style::default().fg(
                    spectrum_band_color(offender.current, &app.settings.graph_heat),
                )),
                Cell::from(format!("{:.1}", offender.avg)),
                Cell::from(format!("{:.1}", offender.peak)),
                Cell::from(format!("{:>5.1}%", offender.share * 100.0)),
            ])
            .style(row_style)
        });

        let header_row = Row::new([
            Cell::from("PROCESS"),
            Cell::from(app.offender_sort.header_label(OffenderSort::Current, "NOW")),
            Cell::from(app.offender_sort.header_label(OffenderSort::Avg2m, "AVG2M")),
            Cell::from(app.offender_sort.header_label(OffenderSort::Peak, "PEAK")),
            Cell::from(app.offender_sort.header_label(OffenderSort::Share, "SHARE")),
        ])
        .style(
            Style::default()
                .fg(COLOR_ACCENT)
                .bg(COLOR_SELECTED_BG)
                .add_modifier(Modifier::BOLD),
        );

        let pin_suffix = offender_pinned
            .as_ref()
            .map(|name| format!(" • pinned {name}"))
            .unwrap_or_default();

        let table_title_right = if app.offender_filter_input.is_some() {
            format!(
                "{offender_visible_len}/{offender_total} groups • filter edit: {} • sort: {}{}",
                app.offender_active_filter(),
                app.offender_sort.title_label(),
                pin_suffix,
            )
        } else if app.offender_filter_query.is_empty() {
            format!(
                "{offender_visible_len}/{offender_total} groups • offender view • sort: {}{}",
                app.offender_sort.title_label(),
                pin_suffix,
            )
        } else {
            format!(
                "{offender_visible_len}/{offender_total} groups • filter: {} • sort: {}{}",
                app.offender_filter_query,
                app.offender_sort.title_label(),
                pin_suffix,
            )
        };

        let highlight_style = if app.is_offender_pinned() {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
                .bg(COLOR_SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        };

        let table_block = panel_block()
            .title_top(
                Line::from(Span::styled(
                    table_title_right,
                    Style::default().fg(COLOR_MUTED),
                ))
                .right_aligned(),
            )
            .title_bottom(hotkey_hint_line(app.status_hint_text()).right_aligned());

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(52),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
            ],
        )
        .header(header_row)
        .block(table_block)
        .column_spacing(1)
        .style(Style::default().fg(COLOR_FG))
        .row_highlight_style(highlight_style);

        let selected_in_window = if offender_visible_len == 0 || app.is_offender_pinned() {
            None
        } else {
            Some(app.offender_selected.saturating_sub(start))
        };
        let mut table_state = TableState::default().with_selected(selected_in_window);
        frame.render_stateful_widget(table, rows_area, &mut table_state);
    }

    let table_chips = vec![
        chip_line("²processes", Some('²')),
        chip_line("³offenders", Some('³')),
    ];
    draw_chips_on_border(
        frame.buffer_mut(),
        rows_area,
        rows_area.y,
        rows_area.x + 1,
        &table_chips,
    );

    let bottom_chips: Vec<Vec<Span<'static>>> = app
        .status_hint_chips()
        .into_iter()
        .map(|(label, hotkey)| chip_line(label, Some(hotkey)))
        .collect();
    if !bottom_chips.is_empty() {
        let bottom_y = rows_area.y + rows_area.height.saturating_sub(1);
        draw_chips_on_border(
            frame.buffer_mut(),
            rows_area,
            bottom_y,
            rows_area.x + 1,
            &bottom_chips,
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

    if inner.width < 12 || inner.height < 7 {
        return;
    }

    let spark_width = (inner.width * 2 / 3).max(20);
    let spark_height: u16 = 5;
    let stack_height = 1 /* power value */ + 1 /* spacer */ + spark_height + 1 /* spacer */ + 1 /* hint */;
    let start_x = inner.x + (inner.width.saturating_sub(spark_width)) / 2;
    let start_y = inner.y + inner.height.saturating_sub(stack_height) / 2;

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

fn snapshot_from_live_snapshot(live_snapshot: &persistence::LiveSnapshot) -> Snapshot {
    let rows = live_snapshot
        .samples
        .iter()
        .map(|sample| ProcRow {
            pid: sample.pid,
            process: sample.process.clone(),
            process_lc: sample.process.to_lowercase(),
            power: format!("{:.1}", sample.power),
            power_num: sample.power,
        })
        .collect();

    snapshot_from_rows(rows)
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
    let power_num = sanitize_power_value(parse_numeric_value(parts.last()?));
    let power = format!("{power_num:.1}");
    let process = parts[1..parts.len().saturating_sub(1)].join(" ");
    let process_lc = process.to_lowercase();

    Some(ProcRow {
        pid,
        process,
        process_lc,
        power_num,
        power,
    })
}

fn parse_numeric_value(s: &str) -> f64 {
    let mut cleaned = String::new();
    let mut seen_digit = false;
    let mut seen_decimal = false;

    for ch in s.trim().chars() {
        if ch.is_ascii_digit() {
            cleaned.push(ch);
            seen_digit = true;
            continue;
        }

        if ch == ',' {
            if seen_digit {
                continue;
            }
            break;
        }

        if ch == '.' && !seen_decimal {
            cleaned.push(ch);
            seen_decimal = true;
            continue;
        }

        if seen_digit || seen_decimal {
            break;
        }
    }

    cleaned.parse::<f64>().unwrap_or(0.0)
}

fn sanitize_power_value(value: f64) -> f64 {
    if value.is_finite() && (0.0..=MAX_REASONABLE_POWER).contains(&value) {
        value
    } else {
        0.0
    }
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
    fn parse_numeric_value_handles_grouping_separators_and_suffix_junk() {
        assert_eq!(parse_numeric_value("1,234.5"), 1234.5);
        assert_eq!(parse_numeric_value("12.3+"), 12.3);
    }

    #[test]
    fn parse_row_sanitizes_implausibly_large_power_values() {
        let row = parse_row("336 powerd 55443258.0").expect("row should parse");
        assert_eq!(row.power_num, 0.0);
        assert_eq!(row.power, "0.0");
    }

    #[test]
    fn history_viewport_samples_keeps_latest_points_without_resampling() {
        let samples = history_viewport_samples(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], 3);
        assert_eq!(samples, vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn history_viewport_samples_shifts_left_as_new_samples_arrive() {
        let width = 3;
        let before = history_viewport_samples(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0], width);
        let after = history_viewport_samples(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0], width);

        assert_eq!(&after[..(width * 2) - 1], &before[1..]);
        assert_eq!(after[(width * 2) - 1], 6.0);
    }

    #[test]
    fn history_viewport_samples_left_pads_short_history_without_faking_plateaus() {
        let samples = history_viewport_samples(&[7.0, 8.0], 4);
        assert_eq!(samples, vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 7.0, 8.0]);
    }

    #[test]
    fn history_viewport_samples_deque_matches_slice_behavior() {
        let values = VecDeque::from(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        let samples = history_viewport_samples_deque(&values, 3);
        assert_eq!(samples, vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn history_viewport_samples_deque_left_pads_short_history_without_faking_plateaus() {
        let values = VecDeque::from(vec![7.0, 8.0]);
        let samples = history_viewport_samples_deque(&values, 4);
        assert_eq!(samples, vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 7.0, 8.0]);
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
    fn value_to_vertical_steps_keeps_low_nonzero_activity_visible() {
        let steps = value_to_vertical_steps(0.1, 0.0, 200.0, 8);
        assert_eq!(steps, 1);
    }

    #[test]
    fn value_to_vertical_steps_keeps_near_zero_blank() {
        let steps = value_to_vertical_steps(GRAPH_ACTIVITY_EPSILON * 0.5, 0.0, 200.0, 8);
        assert_eq!(steps, 0);
    }

    #[test]
    fn braille_history_rows_keeps_low_nonzero_activity_visible() {
        let rows = braille_history_rows(&[0.1, 0.1, 0.1, 0.1], 2, 4);

        let bottom = rows.last().expect("graph should have rows");
        assert_ne!(bottom.chars().nth(0), Some(' '));
        assert_ne!(bottom.chars().nth(1), Some(' '));

        for row in rows.iter().take(rows.len().saturating_sub(1)) {
            assert_eq!(row.chars().nth(0), Some(' '));
            assert_eq!(row.chars().nth(1), Some(' '));
        }
    }

    #[test]
    fn braille_history_rows_uses_baseline_fallback_without_changing_geometry() {
        let rows = braille_history_rows(&[0.1, 0.1], 1, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], " ");
        assert_eq!(rows[1], " ");
        assert_eq!(rows[2], " ");
        assert_ne!(rows[3], " ");
    }

    #[test]
    fn braille_history_rows_treats_near_zero_as_blank() {
        let low = GRAPH_ACTIVITY_EPSILON * 0.5;
        let rows = braille_history_rows(&[low, low, low, low], 2, 4);

        for row in &rows {
            assert_eq!(row.chars().nth(0), Some(' '));
            assert_eq!(row.chars().nth(1), Some(' '));
        }
    }

    #[test]
    fn braille_history_rows_single_row_uses_lookup_mapping() {
        let rows = braille_history_rows(&[10.0], 1, 1);
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

    fn occupied_line_colors(line: &Line<'_>) -> Vec<Color> {
        line.spans
            .iter()
            .filter(|span| span.content.chars().any(|ch| ch != ' '))
            .filter_map(|span| span.style.fg)
            .collect()
    }

    #[test]
    fn row_position_color_bottom_row_is_green() {
        assert_eq!(row_position_color(9, 10), COLOR_GREEN);
        assert_eq!(row_position_color(0, 1), COLOR_GREEN);
    }

    #[test]
    fn row_position_color_top_row_is_red() {
        assert_eq!(row_position_color(0, 10), COLOR_RED);
    }

    #[test]
    fn row_position_color_spans_full_spectrum_over_tall_graph() {
        let colors: Vec<Color> = (0..10).map(|r| row_position_color(r, 10)).collect();
        assert!(colors.contains(&COLOR_RED));
        assert!(colors.contains(&COLOR_ORANGE));
        assert!(colors.contains(&COLOR_YELLOW));
        assert!(colors.contains(&COLOR_GREEN));
    }

    #[test]
    fn graph_row_color_depends_only_on_position_not_column_value() {
        let lines = braille_history_lines(&[80.0, 5.0, 80.0], 2, 8);

        for line in &lines {
            let colors = occupied_line_colors(line);
            if let Some(first) = colors.first().copied() {
                assert!(colors.iter().all(|color| *color == first));
            }
        }
    }

    fn key_press(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), crossterm::event::KeyModifiers::NONE)
    }

    fn key_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
    }

    fn seed_offender_sort_history(app: &mut App) {
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
        app.table_mode = TableMode::Offenders;
    }

    fn offender_names_in_visible_order(app: &mut App) -> Vec<String> {
        app.rebuild_offender_visible_if_needed();
        app.offender_visible_indices
            .iter()
            .map(|idx| app.offender_rows[*idx].name.clone())
            .collect()
    }

    #[test]
    fn offender_sort_cycles_with_s_key_and_wraps() {
        let mut app = App::new();
        app.table_mode = TableMode::Offenders;

        assert_eq!(app.offender_sort, OffenderSort::Current);

        app.handle_key(key_press('s'));
        assert_eq!(app.offender_sort, OffenderSort::Avg2m);

        app.handle_key(key_press('s'));
        assert_eq!(app.offender_sort, OffenderSort::Peak);

        app.handle_key(key_press('s'));
        assert_eq!(app.offender_sort, OffenderSort::Share);

        app.handle_key(key_press('s'));
        assert_eq!(app.offender_sort, OffenderSort::Current);
    }

    #[test]
    fn graph_range_cycles_with_4_key_and_wraps() {
        let mut app = App::new();

        assert_eq!(app.graph_range, GraphRange::Minutes8);

        app.handle_key(key_press('4'));
        assert_eq!(app.graph_range, GraphRange::Minutes30);

        app.handle_key(key_press('4'));
        assert_eq!(app.graph_range, GraphRange::Hours3);

        app.handle_key(key_press('4'));
        assert_eq!(app.graph_range, GraphRange::Hours12);

        app.handle_key(key_press('4'));
        assert_eq!(app.graph_range, GraphRange::Minutes8);
    }

    #[test]
    fn graph_range_8m_uses_existing_live_history_path() {
        let mut app = App::new();
        app.graph_range = GraphRange::Minutes8;
        app.power_history = VecDeque::from(vec![1.0, 2.0, 3.0, 4.0, 5.0]);

        match app.main_graph_samples_for_width(2, 9_999) {
            MainGraphSamples::Live(samples) => {
                assert_eq!(
                    samples,
                    history_viewport_samples_deque(&app.power_history, 2)
                );
            }
            MainGraphSamples::Archive(_) => {
                panic!("8m graph range must stay on live history path");
            }
        }
    }

    #[test]
    fn process_mode_ignores_s_sort_key() {
        let mut app = App::new();
        app.table_mode = TableMode::Processes;
        app.offender_sort = OffenderSort::Peak;

        app.handle_key(key_press('s'));

        assert_eq!(app.offender_sort, OffenderSort::Peak);
    }

    #[test]
    fn enter_toggles_process_pin_in_process_mode() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 4.0),
        ]));
        app.table_mode = TableMode::Processes;

        app.handle_key(key_enter());
        let pinned = app.pinned.as_ref().expect("process should be pinned");
        assert_eq!(pinned.pid, 10);
        assert_eq!(pinned.process, "Safari");

        app.handle_key(key_enter());
        assert!(app.pinned.is_none());
    }

    #[test]
    fn enter_toggles_offender_pin_in_offender_mode() {
        let mut app = App::new();
        seed_offender_sort_history(&mut app);

        let selected_name = app
            .selected_offender_name()
            .expect("selected offender should exist");

        app.handle_key(key_enter());
        assert_eq!(app.offender_pinned.as_deref(), Some(selected_name.as_str()));

        app.handle_key(key_enter());
        assert!(app.offender_pinned.is_none());
    }

    #[test]
    fn offender_enter_does_not_clear_existing_process_pin() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 4.0),
        ]));

        app.table_mode = TableMode::Processes;
        app.handle_key(key_enter());
        let process_pin = app
            .pinned
            .as_ref()
            .expect("process should be pinned")
            .clone();

        app.table_mode = TableMode::Offenders;
        app.handle_key(key_enter());
        assert!(app.offender_pinned.is_some());

        let still_pinned = app
            .pinned
            .as_ref()
            .expect("process pin should remain while offender pinning");
        assert_eq!(still_pinned.pid, process_pin.pid);
        assert_eq!(still_pinned.process, process_pin.process);

        app.handle_key(key_enter());
        assert!(app.offender_pinned.is_none());
        assert!(app.pinned.is_some());
    }

    #[test]
    fn offender_sort_changes_visible_ordering_by_metric() {
        let mut app = App::new();
        seed_offender_sort_history(&mut app);

        assert_eq!(
            offender_names_in_visible_order(&mut app),
            vec![
                "Mail".to_string(),
                "Slack".to_string(),
                "Safari".to_string(),
            ]
        );

        app.cycle_offender_sort();
        assert_eq!(app.offender_sort, OffenderSort::Avg2m);
        assert_eq!(
            offender_names_in_visible_order(&mut app),
            vec![
                "Safari".to_string(),
                "Mail".to_string(),
                "Slack".to_string(),
            ]
        );

        app.cycle_offender_sort();
        assert_eq!(app.offender_sort, OffenderSort::Peak);
        assert_eq!(
            offender_names_in_visible_order(&mut app),
            vec![
                "Safari".to_string(),
                "Mail".to_string(),
                "Slack".to_string(),
            ]
        );

        app.cycle_offender_sort();
        assert_eq!(app.offender_sort, OffenderSort::Share);
        assert_eq!(
            offender_names_in_visible_order(&mut app),
            vec![
                "Mail".to_string(),
                "Slack".to_string(),
                "Safari".to_string(),
            ]
        );
    }

    #[test]
    fn offender_sort_cycle_preserves_selected_name_when_possible() {
        let mut app = App::new();
        seed_offender_sort_history(&mut app);

        app.offender_selected = 1;
        let selected_before = app
            .selected_offender_name()
            .expect("selected offender should exist");
        assert_eq!(selected_before, "Slack");

        app.handle_key(key_press('s'));

        let selected_after = app
            .selected_offender_name()
            .expect("selected offender should still exist");
        assert_eq!(selected_after, "Slack");
        assert_eq!(app.offender_selected, 2);
    }

    #[test]
    fn offender_selection_survives_snapshot_reorder_by_name() {
        let mut app = App::new();
        seed_offender_sort_history(&mut app);

        app.offender_selected = 1;
        assert_eq!(app.selected_offender_name().as_deref(), Some("Slack"));

        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 30.0),
            row(20, "Mail", 10.0),
            row(30, "Slack", 5.0),
        ]));

        assert_eq!(app.selected_offender_name().as_deref(), Some("Slack"));
        assert_eq!(app.offender_selected, 2);
    }

    #[test]
    fn offender_filter_matches_group_name_only() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(101, "Safari", 12.0),
            row(202, "Mail", 4.0),
        ]));

        app.table_mode = TableMode::Offenders;

        app.offender_filter_query = "saf".to_string();
        app.mark_offender_visible_dirty();
        let visible_len = app.offender_visible_len();

        assert_eq!(visible_len, 1);
        let idx = app.offender_visible_indices[0];
        assert_eq!(app.offender_rows[idx].name, "Safari");

        app.offender_filter_query = "101".to_string();
        app.mark_offender_visible_dirty();
        assert_eq!(app.offender_visible_len(), 0);
    }

    #[test]
    fn offender_selection_normalizes_after_filter_changes_result_size() {
        let mut app = App::new();
        app.apply_snapshot(snapshot_from_rows(vec![
            row(10, "Safari", 8.0),
            row(20, "Mail", 6.0),
            row(30, "Slack", 4.0),
        ]));

        app.table_mode = TableMode::Offenders;
        assert_eq!(app.offender_visible_len(), 3);

        app.offender_selected = 2;
        app.offender_scroll = 2;

        app.offender_filter_query = "mail".to_string();
        app.mark_offender_visible_dirty();
        let visible_len = app.offender_visible_len();
        app.normalize_offender_selection(visible_len);

        assert_eq!(visible_len, 1);
        assert_eq!(app.offender_selected, 0);
        assert_eq!(app.offender_scroll, 0);
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
            [("Safari".to_string(), 12.0)],
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
        assert_eq!(app.history_store.name_current("Safari"), 0.0);
        assert_eq!(app.archive, archive);
        assert!(app.loading);
        assert!(app.snapshot.rows.is_empty());
    }
}
