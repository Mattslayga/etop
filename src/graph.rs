use std::collections::VecDeque;

use ratatui::prelude::*;

use crate::archive_query;

pub(crate) const GRAPH_ACTIVITY_EPSILON: f64 = 1e-3;

const COLOR_GREEN: Color = Color::Rgb(0x98, 0xc3, 0x79);
const COLOR_YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const COLOR_ORANGE: Color = Color::Rgb(0xd1, 0x9a, 0x66);
const COLOR_RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);

const BRAILLE_DOT_MASKS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphRange {
    Minutes8,
    Minutes30,
    Hours3,
    Hours12,
}

impl GraphRange {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Minutes8 => Self::Minutes30,
            Self::Minutes30 => Self::Hours3,
            Self::Hours3 => Self::Hours12,
            Self::Hours12 => Self::Minutes8,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Minutes8 => "8m",
            Self::Minutes30 => "30m",
            Self::Hours3 => "3h",
            Self::Hours12 => "12h",
        }
    }

    pub(crate) fn archive_range(self) -> Option<archive_query::ArchiveGraphRange> {
        match self {
            Self::Minutes8 => None,
            Self::Minutes30 => Some(archive_query::ArchiveGraphRange::Minutes30),
            Self::Hours3 => Some(archive_query::ArchiveGraphRange::Hours3),
            Self::Hours12 => Some(archive_query::ArchiveGraphRange::Hours12),
        }
    }
}

pub(crate) fn history_range(values: &[f64]) -> Option<(f64, f64)> {
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

pub(crate) fn history_viewport_samples(values: &[f64], width: usize) -> Vec<f64> {
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

pub(crate) fn history_viewport_samples_deque(values: &VecDeque<f64>, width: usize) -> Vec<f64> {
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

pub(crate) fn history_viewport_samples_optional(
    values: &[Option<f64>],
    width: usize,
) -> Vec<Option<f64>> {
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

pub(crate) fn history_range_optional(values: &[Option<f64>]) -> Option<(f64, f64)> {
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

pub(crate) fn graph_scale_bounds_optional(values: &[Option<f64>]) -> (f64, f64) {
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

pub(crate) fn graph_scale_bounds(values: &[f64]) -> (f64, f64) {
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

pub(crate) fn value_to_vertical_steps(value: f64, min: f64, max: f64, rows: usize) -> i32 {
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

fn graph_span_style(color: Option<Color>) -> Style {
    match color {
        Some(color) => Style::default().fg(color),
        None => Style::default(),
    }
}

fn braille_char_from_bits(bits: u8) -> char {
    if bits == 0 {
        ' '
    } else {
        char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
    }
}

pub(crate) fn row_position_color(row_from_top: usize, height: usize) -> Color {
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

fn value_to_top_dot_row(value: f64, min: f64, max: f64, height: usize) -> Option<usize> {
    let steps = value_to_vertical_steps(value, min, max, height);
    if steps <= 0 {
        return None;
    }

    let dot_height = height.saturating_mul(4);
    Some(dot_height.saturating_sub(steps as usize))
}

fn mark_contour_top(contour_tops: &mut [Option<usize>], x: usize, y: usize) {
    let Some(slot) = contour_tops.get_mut(x) else {
        return;
    };

    match slot {
        Some(current) => *current = (*current).min(y),
        None => *slot = Some(y),
    }
}

fn rasterize_segment_contour_tops(
    contour_tops: &mut [Option<usize>],
    start_x: usize,
    start_y: usize,
    end_x: usize,
    end_y: usize,
) {
    let mut x0 = start_x as isize;
    let mut y0 = start_y as isize;
    let x1 = end_x as isize;
    let y1 = end_y as isize;

    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        mark_contour_top(contour_tops, x0 as usize, y0 as usize);
        if x0 == x1 && y0 == y1 {
            break;
        }

        let err2 = err * 2;
        if err2 >= dy {
            err += dy;
            x0 += sx;
        }
        if err2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn contour_tops_from_points(points: &[Option<usize>]) -> Vec<Option<usize>> {
    let mut contour_tops = vec![None; points.len()];
    let mut previous: Option<(usize, usize)> = None;

    for (x, point) in points.iter().copied().enumerate() {
        match point {
            Some(y) => {
                mark_contour_top(&mut contour_tops, x, y);

                if let Some((prev_x, prev_y)) = previous
                    && prev_x + 1 == x
                {
                    rasterize_segment_contour_tops(&mut contour_tops, prev_x, prev_y, x, y);
                }

                previous = Some((x, y));
            }
            None => previous = None,
        }
    }

    contour_tops
}

fn braille_rows_from_contour_tops(
    contour_tops: &[Option<usize>],
    width: usize,
    height: usize,
) -> Vec<Vec<(char, Color)>> {
    let mut rows = Vec::with_capacity(height);

    for row_from_top in 0..height {
        let row_color = row_position_color(row_from_top, height);
        let dot_row_start = row_from_top * 4;

        let mut line = Vec::with_capacity(width);
        for col in 0..width {
            let mut bits = 0u8;

            for (dy, masks) in BRAILLE_DOT_MASKS.iter().enumerate() {
                let dot_y = dot_row_start + dy;
                let left_x = col * 2;
                let right_x = left_x + 1;

                if contour_tops
                    .get(left_x)
                    .copied()
                    .flatten()
                    .is_some_and(|top_y| dot_y >= top_y)
                {
                    bits |= masks[0];
                }

                if contour_tops
                    .get(right_x)
                    .copied()
                    .flatten()
                    .is_some_and(|top_y| dot_y >= top_y)
                {
                    bits |= masks[1];
                }
            }

            line.push((braille_char_from_bits(bits), row_color));
        }

        rows.push(line);
    }

    rows
}

pub(crate) fn braille_history_cells_with_scale(
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

    let points: Vec<Option<usize>> = samples
        .iter()
        .map(|value| value_to_top_dot_row(*value, scale_min, scale_max, height))
        .collect();
    let contour_tops = contour_tops_from_points(&points);
    braille_rows_from_contour_tops(&contour_tops, width, height)
}

#[cfg(test)]
fn braille_history_cells(values: &[f64], width: usize, height: usize) -> Vec<Vec<(char, Color)>> {
    let samples = history_viewport_samples(values, width);
    let (scale_min, scale_max) = graph_scale_bounds(&samples);
    braille_history_cells_with_scale(values, width, height, scale_min, scale_max)
}

pub(crate) fn braille_history_lines_with_scale(
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

pub(crate) fn braille_history_cells_optional_with_scale(
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

    let points: Vec<Option<usize>> = samples
        .iter()
        .map(|value| {
            value.and_then(|value| value_to_top_dot_row(value, scale_min, scale_max, height))
        })
        .collect();
    let contour_tops = contour_tops_from_points(&points);
    braille_rows_from_contour_tops(&contour_tops, width, height)
}

pub(crate) fn braille_history_lines_optional_with_scale(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn occupied_line_colors(line: &Line<'_>) -> Vec<Color> {
        line.spans
            .iter()
            .filter(|span| span.content.chars().any(|ch| ch != ' '))
            .filter_map(|span| span.style.fg)
            .collect()
    }

    fn occupied_dot_columns(rows: &[String]) -> Vec<Vec<usize>> {
        let height = rows.len();
        if height == 0 {
            return Vec::new();
        }

        let width = rows[0].chars().count();
        let mut columns = vec![Vec::new(); width * 2];

        for (row_idx, row) in rows.iter().enumerate() {
            for (col_idx, ch) in row.chars().enumerate() {
                let bits = if ch == ' ' {
                    0
                } else {
                    (ch as u32).saturating_sub(0x2800) as u8
                };

                for (dy, masks) in BRAILLE_DOT_MASKS.iter().enumerate() {
                    let dot_y = row_idx * 4 + dy;
                    if bits & masks[0] != 0 {
                        columns[col_idx * 2].push(dot_y);
                    }
                    if bits & masks[1] != 0 {
                        columns[col_idx * 2 + 1].push(dot_y);
                    }
                }
            }
        }

        for dots in &mut columns {
            dots.sort_unstable();
            dots.dedup();
        }

        columns
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
    fn braille_history_rows_rasterize_continuous_segments_across_cell_boundaries() {
        let rows = braille_history_rows(&[80.0, 5.0, 80.0, 5.0], 2, 8);
        let dot_columns = occupied_dot_columns(&rows);

        assert!(dot_columns.iter().all(|column| !column.is_empty()));
    }

    #[test]
    fn optional_braille_history_rows_break_segments_on_none_samples() {
        let samples = vec![Some(80.0), Some(80.0), None, Some(80.0)];
        let (scale_min, scale_max) = graph_scale_bounds_optional(&samples);
        let rows = braille_history_cells_optional_with_scale(&samples, 2, 8, scale_min, scale_max)
            .into_iter()
            .map(|row| row.into_iter().map(|(ch, _)| ch).collect::<String>())
            .collect::<Vec<_>>();
        let dot_columns = occupied_dot_columns(&rows);

        assert!(!dot_columns[0].is_empty());
        assert!(!dot_columns[1].is_empty());
        assert!(dot_columns[2].is_empty());
        assert!(!dot_columns[3].is_empty());
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
