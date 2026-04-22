use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PidKey {
    pub pid: i32,
    pub process: String,
}

impl PidKey {
    pub fn new(pid: i32, process: impl Into<String>) -> Self {
        Self {
            pid,
            process: process.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessSample<'a> {
    pub pid: i32,
    pub process: &'a str,
    pub power: f64,
}

#[derive(Debug, Clone)]
struct TimedSample {
    tick: u64,
    value: f64,
}

#[derive(Debug, Clone, Default)]
struct Series {
    samples: VecDeque<TimedSample>,
    last_seen_tick: u64,
}

impl Series {
    fn push(&mut self, tick: u64, value: f64, max_samples: usize) {
        self.samples.push_back(TimedSample { tick, value });
        while self.samples.len() > max_samples {
            self.samples.pop_front();
        }
    }

    fn current(&self) -> f64 {
        self.samples.back().map(|s| s.value).unwrap_or(0.0)
    }

    fn avg_since_tick(&self, min_tick: u64) -> f64 {
        let mut sum = 0.0;
        let mut count = 0usize;

        for sample in self.samples.iter().rev() {
            if sample.tick < min_tick {
                break;
            }
            sum += sample.value;
            count += 1;
        }

        if count == 0 { 0.0 } else { sum / count as f64 }
    }

    fn peak_since_tick(&self, min_tick: u64) -> f64 {
        let mut peak = 0.0_f64;

        for sample in self.samples.iter().rev() {
            if sample.tick < min_tick {
                break;
            }
            peak = peak.max(sample.value);
        }

        peak
    }

    fn recent_values_since_tick(&self, min_tick: u64) -> Vec<f64> {
        let mut out = Vec::new();
        for sample in self.samples.iter().rev() {
            if sample.tick < min_tick {
                break;
            }
            out.push(sample.value);
        }
        out.reverse();
        out
    }
}

#[derive(Debug, Clone)]
pub struct NameOffenderMetrics {
    pub name: String,
    pub current: f64,
    pub avg: f64,
    pub peak: f64,
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    max_samples: usize,
    stale_after_ticks: u64,
    by_pid: HashMap<PidKey, Series>,
    by_name: HashMap<String, Series>,
}

impl HistoryStore {
    pub fn new(max_samples: usize, stale_after_ticks: u64) -> Self {
        Self {
            max_samples: max_samples.max(1),
            stale_after_ticks,
            by_pid: HashMap::new(),
            by_name: HashMap::new(),
        }
    }

    pub fn update<'a, I>(&mut self, tick: u64, samples: I)
    where
        I: IntoIterator<Item = ProcessSample<'a>>,
    {
        let mut seen_pids: HashSet<PidKey> = HashSet::new();
        let mut name_totals: HashMap<String, f64> = HashMap::new();

        for sample in samples {
            let key = PidKey::new(sample.pid, sample.process.to_string());
            let entry = self.by_pid.entry(key.clone()).or_default();
            entry.last_seen_tick = tick;
            entry.push(tick, sample.power, self.max_samples);
            seen_pids.insert(key);

            *name_totals.entry(sample.process.to_string()).or_insert(0.0) += sample.power;
        }

        let seen_names: HashSet<String> = name_totals.keys().cloned().collect();

        for (name, total) in name_totals {
            let entry = self.by_name.entry(name).or_default();
            entry.last_seen_tick = tick;
            entry.push(tick, total, self.max_samples);
        }

        for (key, series) in self.by_pid.iter_mut() {
            if !seen_pids.contains(key) {
                series.push(tick, 0.0, self.max_samples);
            }
        }

        for (name, series) in self.by_name.iter_mut() {
            if !seen_names.contains(name) {
                series.push(tick, 0.0, self.max_samples);
            }
        }

        let stale_after_ticks = self.stale_after_ticks;
        self.by_pid
            .retain(|_, series| tick.saturating_sub(series.last_seen_tick) <= stale_after_ticks);
        self.by_name
            .retain(|_, series| tick.saturating_sub(series.last_seen_tick) <= stale_after_ticks);
    }

    #[allow(dead_code)]
    pub fn pid_current(&self, key: &PidKey) -> f64 {
        self.by_pid.get(key).map(Series::current).unwrap_or(0.0)
    }

    #[allow(dead_code)]
    pub fn pid_avg(&self, key: &PidKey, window_ticks: u64, now_tick: u64) -> f64 {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_pid
            .get(key)
            .map(|series| series.avg_since_tick(min_tick))
            .unwrap_or(0.0)
    }

    #[allow(dead_code)]
    pub fn pid_peak(&self, key: &PidKey, window_ticks: u64, now_tick: u64) -> f64 {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_pid
            .get(key)
            .map(|series| series.peak_since_tick(min_tick))
            .unwrap_or(0.0)
    }

    pub fn pid_recent_values(&self, key: &PidKey, window_ticks: u64, now_tick: u64) -> Vec<f64> {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_pid
            .get(key)
            .map(|series| series.recent_values_since_tick(min_tick))
            .unwrap_or_default()
    }

    pub fn name_current(&self, name: &str) -> f64 {
        self.by_name.get(name).map(Series::current).unwrap_or(0.0)
    }

    pub fn name_avg(&self, name: &str, window_ticks: u64, now_tick: u64) -> f64 {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_name
            .get(name)
            .map(|series| series.avg_since_tick(min_tick))
            .unwrap_or(0.0)
    }

    pub fn name_peak(&self, name: &str, window_ticks: u64, now_tick: u64) -> f64 {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_name
            .get(name)
            .map(|series| series.peak_since_tick(min_tick))
            .unwrap_or(0.0)
    }

    pub fn name_recent_values(&self, name: &str, window_ticks: u64, now_tick: u64) -> Vec<f64> {
        let min_tick = now_tick.saturating_sub(window_ticks.saturating_sub(1));
        self.by_name
            .get(name)
            .map(|series| series.recent_values_since_tick(min_tick))
            .unwrap_or_default()
    }

    pub fn top_name_offenders(
        &self,
        now_tick: u64,
        avg_window_ticks: u64,
        peak_window_ticks: u64,
        limit: usize,
    ) -> Vec<NameOffenderMetrics> {
        let mut offenders: Vec<NameOffenderMetrics> = self
            .by_name
            .keys()
            .map(|name| NameOffenderMetrics {
                name: name.clone(),
                current: self.name_current(name),
                avg: self.name_avg(name, avg_window_ticks, now_tick),
                peak: self.name_peak(name, peak_window_ticks, now_tick),
            })
            .collect();

        offenders.sort_by(|a, b| {
            b.current
                .partial_cmp(&a.current)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.avg
                        .partial_cmp(&a.avg)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.name.cmp(&b.name))
        });

        offenders.truncate(limit);
        offenders
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s<'a>(pid: i32, process: &'a str, power: f64) -> ProcessSample<'a> {
        ProcessSample {
            pid,
            process,
            power,
        }
    }

    #[test]
    fn groups_by_exact_process_name() {
        let mut store = HistoryStore::new(8, 8);

        store.update(1, [s(10, "Safari", 4.0), s(11, "Safari", 2.0)]);

        assert_eq!(store.name_current("Safari"), 6.0);
    }

    #[test]
    fn pid_key_uses_pid_plus_exact_name() {
        let mut store = HistoryStore::new(8, 8);

        store.update(1, [s(10, "Safari", 4.0)]);
        store.update(2, [s(10, "Safari GPU", 8.0)]);

        let safari_key = PidKey::new(10, "Safari");
        let gpu_key = PidKey::new(10, "Safari GPU");

        assert_eq!(store.pid_current(&safari_key), 0.0);
        assert_eq!(store.pid_current(&gpu_key), 8.0);
    }

    #[test]
    fn missing_entries_receive_zero_samples() {
        let mut store = HistoryStore::new(8, 8);
        let key = PidKey::new(10, "Safari");

        store.update(1, [s(10, "Safari", 4.0)]);
        store.update(2, []);

        assert_eq!(store.pid_current(&key), 0.0);
        assert_eq!(store.pid_avg(&key, 2, 2), 2.0);
    }

    #[test]
    fn evicts_stale_pid_and_name_series() {
        let mut store = HistoryStore::new(8, 2);
        let key = PidKey::new(10, "Safari");

        store.update(1, [s(10, "Safari", 4.0)]);
        store.update(2, []);
        store.update(3, []);
        store.update(4, []);

        assert_eq!(store.pid_current(&key), 0.0);
        assert_eq!(store.name_current("Safari"), 0.0);
    }

    #[test]
    fn bounded_history_uses_vecdeque_limit() {
        let mut store = HistoryStore::new(3, 10);

        store.update(1, [s(10, "Safari", 1.0)]);
        store.update(2, [s(10, "Safari", 2.0)]);
        store.update(3, [s(10, "Safari", 3.0)]);
        store.update(4, [s(10, "Safari", 4.0)]);

        let values = store.name_recent_values("Safari", 10, 4);
        assert_eq!(values, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn computes_avg_and_peak() {
        let mut store = HistoryStore::new(16, 16);

        store.update(1, [s(10, "Safari", 2.0)]);
        store.update(2, [s(10, "Safari", 4.0)]);
        store.update(3, [s(10, "Safari", 6.0)]);

        assert_eq!(store.name_avg("Safari", 2, 3), 5.0);
        assert_eq!(store.name_peak("Safari", 3, 3), 6.0);
    }

    #[test]
    fn pid_recent_values_follow_exact_identity() {
        let mut store = HistoryStore::new(16, 16);
        let key = PidKey::new(10, "Safari");

        store.update(1, [s(10, "Safari", 2.0)]);
        store.update(2, [s(10, "Safari", 4.0)]);
        store.update(3, [s(10, "Safari GPU", 7.0)]);

        assert_eq!(store.pid_recent_values(&key, 3, 3), vec![2.0, 4.0, 0.0]);
    }

    #[test]
    fn top_offenders_sorted_by_current_then_avg() {
        let mut store = HistoryStore::new(16, 16);

        store.update(
            1,
            [
                s(10, "Safari", 6.0),
                s(11, "Mail", 3.0),
                s(12, "Slack", 3.0),
            ],
        );
        store.update(
            2,
            [
                s(10, "Safari", 4.0),
                s(11, "Mail", 3.5),
                s(12, "Slack", 3.0),
            ],
        );

        let offenders = store.top_name_offenders(2, 2, 2, 3);
        assert_eq!(offenders.len(), 3);
        assert_eq!(offenders[0].name, "Safari");
        assert_eq!(offenders[1].name, "Mail");
        assert_eq!(offenders[2].name, "Slack");
    }
}
