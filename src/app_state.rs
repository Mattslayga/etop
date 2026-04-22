use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::{
    COLOR_GREEN, COLOR_ORANGE, COLOR_RED, COLOR_YELLOW, CollectorEvent, HISTORY_LIMIT,
    HISTORY_STALE_TICKS, OFFENDER_AVG_WINDOW_TICKS, OFFENDER_PEAK_WINDOW_TICKS, REFRESH_EVERY,
    archive_query,
    graph::{
        GRAPH_ACTIVITY_EPSILON, GraphRange, graph_scale_bounds, history_viewport_samples_deque,
    },
    history::{HistoryStore, NameOffenderMetrics, ProcessSample},
    persistence,
    top_parse::{Snapshot, snapshot_from_live_snapshot},
};
use ratatui::prelude::Color;

#[derive(Debug, Clone)]
pub(crate) struct PinnedProcess {
    pub(crate) pid: i32,
    pub(crate) process: String,
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
        if let Some(edit) = self.editing.as_ref() {
            if edit.field == field {
                return edit.buffer.clone();
            }
        }

        format_setting_value(field.value(&self.draft))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableMode {
    Processes,
    Offenders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffenderSort {
    Current,
    Avg2m,
    Peak,
}

impl OffenderSort {
    fn next(self) -> Self {
        match self {
            Self::Current => Self::Avg2m,
            Self::Avg2m => Self::Peak,
            Self::Peak => Self::Current,
        }
    }

    pub(crate) fn title_label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Avg2m => "avg2m",
            Self::Peak => "peak",
        }
    }

    fn compare(self, a: &NameOffenderMetrics, b: &NameOffenderMetrics) -> Ordering {
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
            .then_with(|| a.name.cmp(&b.name))
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
    pub(crate) offender_pinned: Option<String>,
    pub(crate) process_selected: usize,
    pub(crate) process_scroll: usize,
    pub(crate) process_filter_query: String,
    pub(crate) process_filter_input: Option<String>,
    pub(crate) offender_selected: usize,
    pub(crate) offender_scroll: usize,
    pub(crate) offender_filter_query: String,
    pub(crate) offender_filter_input: Option<String>,
    pub(crate) power_history: VecDeque<f64>,
    pub(crate) live_snapshot_history: VecDeque<persistence::LiveSnapshot>,
    pub(crate) archive: persistence::ArchiveState,
    pub(crate) process_visible_indices: Vec<usize>,
    pub(crate) process_visible_dirty: bool,
    pub(crate) offender_rows: Vec<NameOffenderMetrics>,
    pub(crate) offender_visible_indices: Vec<usize>,
    pub(crate) offender_visible_dirty: bool,
    pub(crate) offender_sort: OffenderSort,
    pub(crate) graph_range: GraphRange,
    pub(crate) show_graph: bool,
    pub(crate) show_table: bool,
    pub(crate) table_mode: TableMode,
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

    pub(crate) fn process_active_filter(&self) -> &str {
        self.process_filter_input
            .as_deref()
            .unwrap_or(self.process_filter_query.as_str())
    }

    pub(crate) fn offender_active_filter(&self) -> &str {
        self.offender_filter_input
            .as_deref()
            .unwrap_or(self.offender_filter_query.as_str())
    }

    pub(crate) fn is_filter_input_active(&self) -> bool {
        match self.table_mode {
            TableMode::Processes => self.process_filter_input.is_some(),
            TableMode::Offenders => self.offender_filter_input.is_some(),
        }
    }

    pub(crate) fn mark_process_visible_dirty(&mut self) {
        self.process_visible_dirty = true;
    }

    pub(crate) fn mark_offender_visible_dirty(&mut self) {
        self.offender_visible_dirty = true;
    }

    pub(crate) fn rebuild_process_visible_if_needed(&mut self) {
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

    pub(crate) fn rebuild_offender_visible_if_needed(&mut self) {
        if !self.offender_visible_dirty {
            return;
        }

        self.offender_rows = self
            .history_store
            .top_name_offenders(
                self.tick,
                OFFENDER_AVG_WINDOW_TICKS,
                OFFENDER_PEAK_WINDOW_TICKS,
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

    pub(crate) fn process_visible_len(&mut self) -> usize {
        self.rebuild_process_visible_if_needed();
        self.process_visible_indices.len()
    }

    pub(crate) fn offender_visible_len(&mut self) -> usize {
        self.rebuild_offender_visible_if_needed();
        self.offender_visible_indices.len()
    }

    fn visible_len(&mut self) -> usize {
        match self.table_mode {
            TableMode::Processes => self.process_visible_len(),
            TableMode::Offenders => self.offender_visible_len(),
        }
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

    pub(crate) fn normalize_offender_selection(&mut self, visible_len: usize) {
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

    pub(crate) fn record_history(&mut self) {
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
            self.mark_process_visible_dirty();
            self.mark_offender_visible_dirty();
            let process_visible_len = self.process_visible_len();
            self.normalize_process_selection(process_visible_len);
            let offender_visible_len = self.offender_visible_len();
            self.normalize_offender_selection(offender_visible_len);
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

    pub(crate) fn offender_archive_samples_for_width(
        &self,
        name: &str,
        graph_width: usize,
    ) -> Option<Vec<Option<f64>>> {
        let archive_range = self.graph_range.archive_range()?;
        Some(archive_query::group_graph_samples_for_range(
            &self.archive,
            archive_range,
            graph_width,
            name,
        ))
    }

    pub(crate) fn graph_scale_bounds_for_viewport(&self, samples: &[f64]) -> (f64, f64) {
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

    pub(crate) fn selected_offender_name(&mut self) -> Option<String> {
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

    pub(crate) fn cycle_offender_sort(&mut self) {
        let selected_name = self.selected_offender_name();
        self.offender_sort = self.offender_sort.next();
        self.mark_offender_visible_dirty();
        self.restore_offender_selection_by_name(selected_name);
    }

    pub(crate) fn is_pinned(&self) -> bool {
        self.is_process_table_mode() && self.pinned.is_some()
    }

    pub(crate) fn is_offender_pinned(&self) -> bool {
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

    pub(crate) fn apply_snapshot(&mut self, next: Snapshot) {
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

    pub(crate) fn apply_collector_event(&mut self, event: CollectorEvent) {
        match event {
            CollectorEvent::Snapshot(next) => self.apply_snapshot(next),
            CollectorEvent::Error(err) => self.apply_refresh_error(err),
        }
    }

    pub(crate) fn status_hint_text(&self) -> &'static str {
        if self.settings_modal.is_some() {
            "Enter edit • Esc cancel"
        } else if self.is_filter_input_active() {
            "Enter apply • Esc cancel"
        } else if self.table_mode == TableMode::Offenders {
            if self.offender_pinned.is_some() {
                "Enter unpin • 4 range"
            } else {
                "Enter pin • s sort"
            }
        } else if self.pinned.is_some() {
            "Enter unpin"
        } else {
            "Enter pin"
        }
    }

    pub(crate) fn status_hint_chips(&self) -> Vec<(&'static str, char)> {
        if self.settings_modal.is_some() {
            vec![("menu", 'm')]
        } else if self.is_filter_input_active() {
            vec![]
        } else {
            vec![("menu", 'm'), ("filter /", '/'), ("quit", 'q')]
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
            KeyCode::Char('p') | KeyCode::Char('P') => self.toggle_pause(),
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
