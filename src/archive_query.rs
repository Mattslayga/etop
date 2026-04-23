use crate::{
    history::PidKey,
    persistence::{
        AGG_10S_BUCKET_SECS, AGG_60S_BUCKET_SECS, ArchiveState, RAW_2S_BUCKET_SECS, TierSample,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveGraphRange {
    Minutes30,
    Hours3,
    Hours12,
}

impl ArchiveGraphRange {
    fn window_secs(self) -> u64 {
        match self {
            Self::Minutes30 => 30 * 60,
            Self::Hours3 => 3 * 60 * 60,
            Self::Hours12 => 12 * 60 * 60,
        }
    }

    fn bucket_secs(self) -> u64 {
        match self {
            Self::Minutes30 => RAW_2S_BUCKET_SECS,
            Self::Hours3 => AGG_10S_BUCKET_SECS,
            Self::Hours12 => AGG_60S_BUCKET_SECS,
        }
    }
}

#[allow(dead_code)]
pub(crate) fn graph_samples_for_range(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
    viewport_width: usize,
) -> Vec<Option<f64>> {
    graph_samples_for_range_with(archive, range, viewport_width, |tier_sample| {
        if tier_sample.sample_count == 0 {
            0.0
        } else {
            tier_sample.total_power_sum / f64::from(tier_sample.sample_count)
        }
    })
}

pub(crate) fn pid_graph_samples_for_range(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
    viewport_width: usize,
    key: &PidKey,
) -> Vec<Option<f64>> {
    graph_samples_for_range_with(archive, range, viewport_width, |tier_sample| {
        if tier_sample.sample_count == 0 {
            return 0.0;
        }

        tier_sample
            .processes
            .iter()
            .find(|process| process.pid == key.pid && process.process == key.process)
            .map(|process| process.power_sum / f64::from(tier_sample.sample_count))
            .unwrap_or(0.0)
    })
}

fn graph_samples_for_range_with<F>(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
    viewport_width: usize,
    sample_value: F,
) -> Vec<Option<f64>>
where
    F: Fn(&TierSample) -> f64,
{
    let points = viewport_width.saturating_mul(2);
    if points == 0 {
        return Vec::new();
    }

    let Some(window_end_bucket) = anchored_window_end_bucket(archive, range, points) else {
        return vec![None; points];
    };

    let window_start_bucket = window_end_bucket.saturating_sub(points as u64);

    #[derive(Default)]
    struct GraphBin {
        sum: f64,
        count: u32,
        force_gap: bool,
    }

    #[derive(Clone, Copy)]
    struct PrevPoint {
        display_bucket: u64,
        bin_idx: usize,
    }

    let mut bins: Vec<GraphBin> = std::iter::repeat_with(GraphBin::default)
        .take(points)
        .collect();

    let mut prev: Option<PrevPoint> = None;
    for tier_sample in tier_for_range(archive, range) {
        let display_bucket = display_bucket_for_sample(
            tier_sample.bucket_start_secs,
            range.bucket_secs(),
            range.window_secs(),
            points,
        );

        if display_bucket < window_start_bucket || display_bucket >= window_end_bucket {
            continue;
        }

        let avg = sample_value(tier_sample);
        let mut target_idx = (display_bucket - window_start_bucket) as usize;

        if let Some(prev_point) = prev {
            target_idx = target_idx.max(prev_point.bin_idx);

            let bucket_gap = display_bucket > prev_point.display_bucket.saturating_add(1);
            if tier_sample.gap_before || bucket_gap {
                let gap_idx = target_idx.saturating_sub(1).min(points - 1);
                bins[gap_idx].force_gap = true;

                if target_idx == prev_point.bin_idx && target_idx + 1 < points {
                    target_idx += 1;
                }
            }
        }

        bins[target_idx].sum += avg;
        bins[target_idx].count = bins[target_idx].count.saturating_add(1);
        prev = Some(PrevPoint {
            display_bucket,
            bin_idx: target_idx,
        });
    }

    bins.into_iter()
        .map(|bin| {
            if bin.force_gap || bin.count == 0 {
                None
            } else {
                Some(bin.sum / f64::from(bin.count))
            }
        })
        .collect()
}

fn anchored_window_end_bucket(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
    points: usize,
) -> Option<u64> {
    tier_for_range(archive, range).back().map(|sample| {
        display_bucket_for_sample(
            sample.bucket_start_secs,
            range.bucket_secs(),
            range.window_secs(),
            points,
        )
        .saturating_add(1)
    })
}

fn tier_for_range(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
) -> &std::collections::VecDeque<TierSample> {
    match range {
        ArchiveGraphRange::Minutes30 => &archive.raw_2s,
        ArchiveGraphRange::Hours3 => &archive.agg_10s,
        ArchiveGraphRange::Hours12 => &archive.agg_60s,
    }
}

fn display_bucket_for_sample(
    bucket_start_secs: u64,
    sample_bucket_secs: u64,
    window_secs: u64,
    bins: usize,
) -> u64 {
    if bins == 0 || window_secs == 0 {
        return 0;
    }

    let midpoint_numer = u128::from(bucket_start_secs)
        .saturating_mul(2)
        .saturating_add(u128::from(sample_bucket_secs));
    let idx =
        (midpoint_numer.saturating_mul(bins as u128)) / u128::from(window_secs).saturating_mul(2);
    idx.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::persistence::ArchivedProcessPower;

    #[test]
    fn graph_samples_for_range_uses_averages_across_full_window() {
        let archive = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 10,
                    sample_count: 2,
                    total_power_sum: 20.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 1_700,
                    sample_count: 1,
                    total_power_sum: 30.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
            ]),
            ..ArchiveState::default()
        };

        let samples = graph_samples_for_range(&archive, ArchiveGraphRange::Minutes30, 3);

        assert_eq!(samples.len(), 6);
        assert_eq!(samples[0], Some(10.0));
        assert_eq!(samples[5], Some(30.0));
        assert!(samples[1..5].iter().all(|value| value.is_none()));
    }

    #[test]
    fn graph_samples_for_range_preserves_discontinuity_with_gap_bin() {
        let archive = ArchiveState {
            agg_10s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 12.0,
                    processes: Vec::new(),
                    gap_before: true,
                },
            ]),
            ..ArchiveState::default()
        };

        let samples = graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 4);

        assert_eq!(samples.len(), 8);
        assert_eq!(samples[0], None);
        assert_eq!(samples[1], Some(12.0));
    }

    #[test]
    fn graph_samples_for_range_stays_anchored_to_latest_bucket_boundary() {
        let archive = ArchiveState {
            agg_10s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 10_770,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 10_790,
                    sample_count: 2,
                    total_power_sum: 30.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
            ]),
            ..ArchiveState::default()
        };

        let samples_a = graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 4);
        let samples_b = graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 4);

        assert_eq!(samples_a, samples_b);
        assert_eq!(samples_a[7], Some(12.5));
    }

    #[test]
    fn pid_graph_samples_for_range_uses_exact_pid_identity() {
        let archive = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 10,
                    sample_count: 2,
                    total_power_sum: 20.0,
                    processes: vec![
                        ArchivedProcessPower {
                            pid: 10,
                            process: "Safari".to_string(),
                            power_sum: 12.0,
                        },
                        ArchivedProcessPower {
                            pid: 20,
                            process: "Mail".to_string(),
                            power_sum: 8.0,
                        },
                    ],
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 1_700,
                    sample_count: 2,
                    total_power_sum: 30.0,
                    processes: vec![ArchivedProcessPower {
                        pid: 20,
                        process: "Mail".to_string(),
                        power_sum: 4.0,
                    }],
                    gap_before: false,
                },
            ]),
            ..ArchiveState::default()
        };

        let safari_key = PidKey::new(10, "Safari");
        let mail_key = PidKey::new(20, "Mail");
        let samples =
            pid_graph_samples_for_range(&archive, ArchiveGraphRange::Minutes30, 3, &safari_key);
        let mail_samples =
            pid_graph_samples_for_range(&archive, ArchiveGraphRange::Minutes30, 3, &mail_key);

        assert_eq!(samples.len(), 6);
        assert_eq!(samples[0], Some(6.0));
        assert!(samples[1..5].iter().all(|value| value.is_none()));
        assert_eq!(samples[5], Some(0.0));
        assert_eq!(mail_samples[0], Some(4.0));
        assert_eq!(mail_samples[5], Some(2.0));
    }

    #[test]
    fn pid_graph_samples_for_range_preserves_real_discontinuity_gaps() {
        let archive = ArchiveState {
            agg_10s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: vec![ArchivedProcessPower {
                        pid: 10,
                        process: "Safari".to_string(),
                        power_sum: 7.0,
                    }],
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 12.0,
                    processes: vec![ArchivedProcessPower {
                        pid: 10,
                        process: "Safari".to_string(),
                        power_sum: 9.0,
                    }],
                    gap_before: true,
                },
            ]),
            ..ArchiveState::default()
        };

        let key = PidKey::new(10, "Safari");
        let samples = pid_graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 4, &key);

        assert_eq!(samples.len(), 8);
        assert_eq!(samples[0], None);
        assert_eq!(samples[1], Some(9.0));
    }

    #[test]
    fn graph_samples_for_range_respects_explicit_gap_before_adjacent_bucket() {
        let archive = ArchiveState {
            agg_10s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: 110,
                    sample_count: 1,
                    total_power_sum: 12.0,
                    processes: Vec::new(),
                    gap_before: true,
                },
            ]),
            ..ArchiveState::default()
        };

        let samples = graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 100);

        assert_eq!(samples[1], None);
        assert_eq!(samples[2], Some(12.0));
    }

    #[test]
    fn graph_samples_for_range_keeps_closed_columns_stable_within_display_bucket() {
        let width = 90;
        let base = 1_800_000;

        let before = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: base + 90,
                    sample_count: 1,
                    total_power_sum: 4.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 102,
                    sample_count: 1,
                    total_power_sum: 20.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 104,
                    sample_count: 1,
                    total_power_sum: 30.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
            ]),
            ..ArchiveState::default()
        };

        let mut after = before.clone();
        after.raw_2s.push_back(TierSample {
            bucket_start_secs: base + 106,
            sample_count: 1,
            total_power_sum: 40.0,
            processes: Vec::new(),
            gap_before: false,
        });

        let before_samples = graph_samples_for_range(&before, ArchiveGraphRange::Minutes30, width);
        let after_samples = graph_samples_for_range(&after, ArchiveGraphRange::Minutes30, width);

        let diffs: Vec<usize> = before_samples
            .iter()
            .zip(after_samples.iter())
            .enumerate()
            .filter_map(|(idx, (before, after))| (before != after).then_some(idx))
            .collect();

        let current_idx = before_samples
            .iter()
            .rposition(|value| value.is_some())
            .expect("expected active display bucket");
        assert_eq!(diffs, vec![current_idx]);
    }

    #[test]
    fn graph_samples_for_range_only_shifts_when_crossing_display_bucket_boundary() {
        let width = 90;
        let base = 1_800_000;

        let before = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: base + 90,
                    sample_count: 1,
                    total_power_sum: 4.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 102,
                    sample_count: 1,
                    total_power_sum: 20.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 104,
                    sample_count: 1,
                    total_power_sum: 30.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
                TierSample {
                    bucket_start_secs: base + 106,
                    sample_count: 1,
                    total_power_sum: 40.0,
                    processes: Vec::new(),
                    gap_before: false,
                },
            ]),
            ..ArchiveState::default()
        };

        let mut after = before.clone();
        after.raw_2s.push_back(TierSample {
            bucket_start_secs: base + 110,
            sample_count: 1,
            total_power_sum: 50.0,
            processes: Vec::new(),
            gap_before: false,
        });

        let before_samples = graph_samples_for_range(&before, ArchiveGraphRange::Minutes30, width);
        let after_samples = graph_samples_for_range(&after, ArchiveGraphRange::Minutes30, width);

        assert_eq!(
            &after_samples[..after_samples.len() - 1],
            &before_samples[1..]
        );
    }
}
