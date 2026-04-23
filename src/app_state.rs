use std::{cmp::Ordering, collections::VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::prelude::Color;

use crate::{
    COLOR_GREEN, COLOR_ORANGE, COLOR_RED, COLOR_YELLOW, CollectorEvent, HISTORY_LIMIT,
    HISTORY_STALE_TICKS, PROCESS_AVG_WINDOW_TICKS, PROCESS_PEAK_WINDOW_TICKS, REFRESH_EVERY,
    archive_query,
    graph::{GraphRange, graph_scale_bounds, history_viewport_samples_deque},
    history::{HistoryStore, PidKey, ProcessSample},
    persistence,
    top_parse::{Snapshot, snapshot_from_live_snapshot},
};

#[derive(Debug, Clone)]
pub(crate) struct PinnedProcess {
    pub(crate) pid: i32,
    pub(crate) process: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProcessTableRow {
    pub(crate) key: PidKey,
    pub(crate) current: f64,
    pub(crate) avg: f64,
    pub(crate) peak: f64,
    process_lc: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessSort {
    Current,
    Avg2m,
    Peak,
}

impl ProcessSort {
    fn next(self) -> Self {
        match self {
            Self::Current => Self::Avg2m,
            Self::Avg2m => Self::Peak,
            Self::Peak => Self::Current,
        }
    }

    fn compare(self, a: &ProcessTableRow, b: &ProcessTableRow) -> Ordering {
        let primary = match self {
            Self::Current => b.current.partial_cmp(&a.current),
            Self::Avg2m => b.avg.partial_cmp(&a.avg),
            Self::Peak => b.peak.partial_cmp(&a.peak),
        }
        .unwrap_or(Ordering::Equal);

        primary
            .then_with(|| b.current.partial_cmp(&a.current).unwrap_or(Ordering::Equal))
            .then_with(|| b.avg.partial_cmp(&a.avg).unwrap_or(Ordering::Equal))
            .then_with(|| b.peak.partial_cmp(&a.peak).unwrap_or(Ordering::Equal))
            .then_with(|| a.key.process.cmp(&b.key.process))
            .then_with(|| a.key.pid.cmp(&b.key.pid))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GraphHeatSettings {
    pub(crate) yellow_start: f64,
    pub(crate) orange_start: f64,
    pub(crate) red_start: f64,
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
    pub(crate) fn validate(&self) -> Result<(), String> {
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

    pub(crate) fn color_for_power(&self, power: f64) -> Color {
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
pub(crate) struct AppSettings {
    pub(crate) graph_heat: GraphHeatSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsField {
    YellowStart,
    OrangeStart,
    RedStart,
}

pub(crate) const SETTINGS_FIELDS: [SettingsField; 3] = [
    SettingsField::YellowStart,
    SettingsField::OrangeStart,
    SettingsField::RedStart,
];

impl SettingsField {
    pub(crate) fn label(self) -> &'static str {
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
pub(crate) struct SettingsEditState {
    pub(crate) field: SettingsField,
    pub(crate) buffer: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsModalState {
    pub(crate) draft: AppSettings,
    pub(crate) selected: usize,
    pub(crate) editing: Option<SettingsEditState>,
    pub(crate) error: Option<String>,
}

impl SettingsModalState {
    pub(crate) fn new(current: &AppSettings) -> Self {
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

    pub(crate) fn display_value(&self, field: SettingsField) -> String {
        if let Some(edit) = self.editing.as_ref()
            && edit.field == field
        {
            return edit.buffer.clone();
        }

        format_setting_value(field.value(&self.draft))
    }
}

pub(crate) struct App {
    pub(crate) snapshot: Snapshot,
    pub(crate) last_error: Option<String>,
    pub(crate) loading: bool,
    pub(crate) paused: bool,
    pub(crate) settings: AppSettings,
    pub(crate) settings_modal: Option<SettingsModalState>,
    pub(crate) pinned: Option<PinnedProcess>,
    pub(crate) process_selected: usize,
    pub(crate) process_scroll: usize,
    pub(crate) process_filter_query: String,
    pub(crate) process_filter_input: Option<String>,
    pub(crate) process_rows: Vec<ProcessTableRow>,
    pub(crate) process_rows_dirty: bool,
    pub(crate) process_sort: ProcessSort,
    pub(crate) power_history: VecDeque<f64>,
    pub(crate) live_snapshot_history: VecDeque<persistence::LiveSnapshot>,
    pub(crate) archive: persistence::ArchiveState,
    pub(crate) graph_range: GraphRange,
    pub(crate) show_graph: bool,
    pub(crate) show_table: bool,
    pub(crate) tick: u64,
    pub(crate) history_store: HistoryStore,
}

impl App {
    pub(crate) fn new() -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            last_error: None,
            loading: true,
            paused: false,
            settings: AppSettings::default(),
            settings_modal: None,
            pinned: None,
            process_selected: 0,
            process_scroll: 0,
            process_filter_query: String::new(),
            process_filter_input: None,
            process_rows: Vec::new(),
            process_rows_dirty: true,
            process_sort: ProcessSort::Current,
            power_history: VecDeque::with_capacity(HISTORY_LIMIT),
            live_snapshot_history: VecDeque::with_capacity(HISTORY_LIMIT),
            archive: persistence::ArchiveState::default(),
            graph_range: GraphRange::Minutes8,
            show_graph: true,
            show_table: true,
            tick: 0,
            history_store: HistoryStore::new(HISTORY_LIMIT, HISTORY_STALE_TICKS),
        };

        let process_visible_len = app.process_visible_len();
        app.normalize_process_selection(process_visible_len);
        app
    }

    pub(crate) fn process_active_filter(&self) -> &str {
        self.process_filter_input
            .as_deref()
            .unwrap_or(self.process_filter_query.as_str())
    }

    pub(crate) fn is_filter_input_active(&self) -> bool {
        self.process_filter_input.is_some()
    }

    pub(crate) fn mark_process_rows_dirty(&mut self) {
        self.process_rows_dirty = true;
    }

    pub(crate) fn rebuild_process_rows_if_needed(&mut self) {
        if !self.process_rows_dirty {
            return;
        }

        let filter_lc = self.process_active_filter().trim().to_lowercase();
        let has_filter = !filter_lc.is_empty();

        self.process_rows.clear();
        self.process_rows.reserve(self.snapshot.rows.len());

        for row in &self.snapshot.rows {
            let key = PidKey::new(row.pid, row.process.clone());
            let process_row = ProcessTableRow {
                avg: self
                    .history_store
                    .pid_avg(&key, PROCESS_AVG_WINDOW_TICKS, self.tick),
                peak: self
                    .history_store
                    .pid_peak(&key, PROCESS_PEAK_WINDOW_TICKS, self.tick),
                current: row.power_num,
                process_lc: row.process_lc.clone(),
                key,
            };

            let matches = if !has_filter {
                true
            } else {
                process_row.process_lc.contains(&filter_lc)
                    || process_row.key.pid.to_string().contains(&filter_lc)
            };

            if matches {
                self.process_rows.push(process_row);
            }
        }

        self.process_rows
            .sort_by(|a, b| self.process_sort.compare(a, b));
        self.process_rows_dirty = false;
    }

    pub(crate) fn process_visible_len(&mut self) -> usize {
        self.rebuild_process_rows_if_needed();
        self.process_rows.len()
    }

    pub(crate) fn normalize_process_selection(&mut self, visible_len: usize) {
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

    pub(crate) fn selected_process_key(&mut self) -> Option<PidKey> {
        self.rebuild_process_rows_if_needed();
        self.process_rows
            .get(self.process_selected)
            .map(|row| row.key.clone())
    }

    fn restore_process_selection_by_key(&mut self, selected_key: Option<PidKey>) {
        self.rebuild_process_rows_if_needed();

        if let Some(key) = selected_key
            && let Some(next_selected) = self.process_rows.iter().position(|row| row.key == key)
        {
            self.process_selected = next_selected;
        }

        self.normalize_process_selection(self.process_rows.len());
    }

    pub(crate) fn cycle_process_sort(&mut self) {
        let selected_key = self.selected_process_key();
        self.process_sort = self.process_sort.next();
        self.mark_process_rows_dirty();
        self.restore_process_selection_by_key(selected_key);
    }

    pub(crate) fn record_history(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.power_history.push_back(self.snapshot.total_power);

        while self.power_history.len() > HISTORY_LIMIT {
            let _ = self.power_history.pop_front();
        }

        let live_samples: Vec<persistence::LiveProcessSample> = self
            .snapshot
            .rows
            .iter()
            .map(|row| persistence::LiveProcessSample {
                pid: row.pid,
                process: row.process.clone(),
                power: row.power_num,
            })
            .collect();

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
                samples: live_samples.clone(),
            });
        while self.live_snapshot_history.len() > HISTORY_LIMIT {
            let _ = self.live_snapshot_history.pop_front();
        }

        self.archive.record_sample(
            persistence::unix_time_secs_now(),
            persistence::continuity_threshold_secs(REFRESH_EVERY),
            self.snapshot.total_power,
            live_samples,
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

    pub(crate) fn apply_loaded_session_cache(&mut self, loaded: persistence::LoadedSessionCache) {
        let persistence::LoadedSessionCache {
            mut cache,
            gap_millis: _gap_millis,
            hydrate_live,
        } = loaded;

        self.archive = cache.archive;

        if !hydrate_live {
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
            self.mark_process_rows_dirty();
            let process_visible_len = self.process_visible_len();
            self.normalize_process_selection(process_visible_len);
        }
    }

    pub(crate) fn to_session_cache(&self) -> persistence::SessionCache {
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

    pub(crate) fn main_graph_live_samples_for_width(&self, graph_width: usize) -> Vec<f64> {
        history_viewport_samples_deque(&self.power_history, graph_width)
    }

    pub(crate) fn pid_archive_samples_for_width(
        &self,
        key: &PidKey,
        graph_width: usize,
    ) -> Option<Vec<Option<f64>>> {
        let archive_range = self.graph_range.archive_range()?;
        Some(archive_query::pid_graph_samples_for_range(
            &self.archive,
            archive_range,
            graph_width,
            key,
        ))
    }

    pub(crate) fn graph_scale_bounds_for_viewport(&self, samples: &[f64]) -> (f64, f64) {
        graph_scale_bounds(samples)
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.process_visible_len();
        if len == 0 {
            self.process_selected = 0;
            self.process_scroll = 0;
            return;
        }

        let max = (len - 1) as isize;
        self.process_selected = (self.process_selected as isize + delta).clamp(0, max) as usize;
    }

    fn select_top(&mut self) {
        self.process_selected = 0;
        self.process_scroll = 0;
    }

    fn select_bottom(&mut self) {
        let len = self.process_visible_len();
        if len == 0 {
            self.process_selected = 0;
            self.process_scroll = 0;
        } else {
            self.process_selected = len - 1;
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    pub(crate) fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    fn toggle_pin(&mut self) {
        if self.pinned.is_some() {
            self.pinned = None;
            return;
        }

        self.rebuild_process_rows_if_needed();
        let Some(row) = self.process_rows.get(self.process_selected) else {
            return;
        };

        self.pinned = Some(PinnedProcess {
            pid: row.key.pid,
            process: row.key.process.clone(),
        });
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
                KeyCode::Esc => close_modal = true,
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
                KeyCode::Enter => modal.start_edit(),
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
        self.process_filter_input = Some(self.process_filter_query.clone());
        self.mark_process_rows_dirty();
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
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
            self.mark_process_rows_dirty();
        }

        let visible_len = self.process_visible_len();
        self.normalize_process_selection(visible_len);
    }

    pub(crate) fn apply_snapshot(&mut self, next: Snapshot) {
        let selected_key = self.selected_process_key();

        self.snapshot = next;
        self.last_error = None;
        self.loading = false;
        self.record_history();
        self.mark_process_rows_dirty();
        self.restore_process_selection_by_key(selected_key);
    }

    fn apply_refresh_error(&mut self, err: String) {
        self.loading = false;
        self.last_error = Some(format!("refresh failed: {err}"));
    }

    pub(crate) fn apply_collector_event(&mut self, event: CollectorEvent) {
        match event {
            CollectorEvent::Snapshot(next) => self.apply_snapshot(next),
            CollectorEvent::Error(err) => self.apply_refresh_error(err),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
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

        let can_navigate = !self.is_pinned();

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return true,
            KeyCode::Char('j') | KeyCode::Down if can_navigate => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up if can_navigate => self.move_selection(-1),
            KeyCode::Char('g') if can_navigate => self.select_top(),
            KeyCode::Char('G') if can_navigate => self.select_bottom(),
            KeyCode::Char('f') | KeyCode::Char('F') if self.show_table => self.start_filter_input(),
            KeyCode::Char('p') | KeyCode::Char('P') => self.toggle_pause(),
            KeyCode::Char('m') | KeyCode::Char('M') => self.open_settings_modal(),
            KeyCode::Char('1') => self.show_graph = !self.show_graph,
            KeyCode::Char('2') => self.show_table = !self.show_table,
            KeyCode::Char('r') | KeyCode::Char('R') => self.cycle_graph_range(),
            KeyCode::Char('s') | KeyCode::Char('S') => self.cycle_process_sort(),
            KeyCode::Enter => self.toggle_pin(),
            _ => {}
        }

        let visible_len = self.process_visible_len();
        self.normalize_process_selection(visible_len);
        false
    }
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
