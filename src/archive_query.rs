use crate::persistence::{
    AGG_10S_BUCKET_SECS, AGG_60S_BUCKET_SECS, ArchiveState, RAW_2S_BUCKET_SECS, TierSample,
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

pub(crate) fn group_graph_samples_for_range(
    archive: &ArchiveState,
    range: ArchiveGraphRange,
    viewport_width: usize,
    process_name: &str,
) -> Vec<Option<f64>> {
    graph_samples_for_range_with(archive, range, viewport_width, |tier_sample| {
        if tier_sample.sample_count == 0 {
            return 0.0;
        }

        tier_sample
            .groups
            .iter()
            .find(|group| group.name == process_name)
            .map(|group| group.power_sum / f64::from(tier_sample.sample_count))
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

    let Some(window_end_exclusive) = anchored_window_end_exclusive(archive, range) else {
        return vec![None; points];
    };

    let window_secs = range.window_secs().max(1);
    let window_start = window_end_exclusive.saturating_sub(window_secs);

    #[derive(Default)]
    struct GraphBin {
        sum: f64,
        count: u32,
        force_gap: bool,
    }

    #[derive(Clone, Copy)]
    struct PrevPoint {
        bucket_start_secs: u64,
        bin_idx: usize,
    }

    let mut bins: Vec<GraphBin> = std::iter::repeat_with(GraphBin::default)
        .take(points)
        .collect();

    let mut prev: Option<PrevPoint> = None;
    for tier_sample in tier_for_range(archive, range) {
        let bucket_start = tier_sample.bucket_start_secs;
        if bucket_start < window_start || bucket_start >= window_end_exclusive {
            continue;
        }

        let avg = sample_value(tier_sample);

        let mut target_idx = time_to_bin_idx(bucket_start, window_start, window_secs, points);

        if let Some(prev_point) = prev {
            target_idx = target_idx.max(prev_point.bin_idx);

            let contiguous = bucket_start
                == prev_point
                    .bucket_start_secs
                    .saturating_add(range.bucket_secs());

            if !contiguous {
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
            bucket_start_secs: bucket_start,
            bin_idx: target_idx,
        });
    }

    bins.into_iter()
        .map(|bin| {
            if bin.force_gap {
                None
            } else if bin.count == 0 {
                None
            } else {
                Some(bin.sum / f64::from(bin.count))
            }
        })
        .collect()
}

fn anchored_window_end_exclusive(archive: &ArchiveState, range: ArchiveGraphRange) -> Option<u64> {
    tier_for_range(archive, range)
        .back()
        .map(|sample| sample.bucket_start_secs.saturating_add(range.bucket_secs()))
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

fn time_to_bin_idx(
    timestamp_secs: u64,
    window_start_secs: u64,
    window_secs: u64,
    bins: usize,
) -> usize {
    if bins == 0 || window_secs == 0 {
        return 0;
    }

    let offset = timestamp_secs
        .saturating_sub(window_start_secs)
        .min(window_secs.saturating_sub(1));

    let idx = ((u128::from(offset) * bins as u128) / u128::from(window_secs)) as usize;
    idx.min(bins.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[test]
    fn graph_samples_for_range_uses_averages_across_full_window() {
        let archive = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 10,
                    sample_count: 2,
                    total_power_sum: 20.0,
                    groups: Vec::new(),
                },
                TierSample {
                    bucket_start_secs: 1_700,
                    sample_count: 1,
                    total_power_sum: 30.0,
                    groups: Vec::new(),
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
                    groups: Vec::new(),
                },
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 12.0,
                    groups: Vec::new(),
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
                    groups: Vec::new(),
                },
                TierSample {
                    bucket_start_secs: 10_790,
                    sample_count: 2,
                    total_power_sum: 30.0,
                    groups: Vec::new(),
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
    fn group_graph_samples_for_range_uses_grouped_averages_and_zero_for_absent_name() {
        let archive = ArchiveState {
            raw_2s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 10,
                    sample_count: 2,
                    total_power_sum: 20.0,
                    groups: vec![crate::persistence::GroupPowerSum {
                        name: "Safari".to_string(),
                        power_sum: 12.0,
                    }],
                },
                TierSample {
                    bucket_start_secs: 1_700,
                    sample_count: 2,
                    total_power_sum: 30.0,
                    groups: vec![crate::persistence::GroupPowerSum {
                        name: "Mail".to_string(),
                        power_sum: 4.0,
                    }],
                },
            ]),
            ..ArchiveState::default()
        };

        let samples =
            group_graph_samples_for_range(&archive, ArchiveGraphRange::Minutes30, 3, "Safari");

        assert_eq!(samples.len(), 6);
        assert_eq!(samples[0], Some(6.0));
        assert!(samples[1..5].iter().all(|value| value.is_none()));
        assert_eq!(samples[5], Some(0.0));
    }

    #[test]
    fn group_graph_samples_for_range_preserves_real_discontinuity_gaps() {
        let archive = ArchiveState {
            agg_10s: VecDeque::from(vec![
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 10.0,
                    groups: vec![crate::persistence::GroupPowerSum {
                        name: "Safari".to_string(),
                        power_sum: 7.0,
                    }],
                },
                TierSample {
                    bucket_start_secs: 100,
                    sample_count: 1,
                    total_power_sum: 12.0,
                    groups: vec![crate::persistence::GroupPowerSum {
                        name: "Safari".to_string(),
                        power_sum: 9.0,
                    }],
                },
            ]),
            ..ArchiveState::default()
        };

        let samples =
            group_graph_samples_for_range(&archive, ArchiveGraphRange::Hours3, 4, "Safari");

        assert_eq!(samples.len(), 8);
        assert_eq!(samples[0], None);
        assert_eq!(samples[1], Some(9.0));
    }
}
